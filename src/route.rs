//! Workspace mode's decisions that need no I/O: which document a client
//! message names, the documents the client holds open, which root was used
//! last, the ids fleet-lsp renumbers, and the refusal texts.
//!
//! A session started outside any git repository serves every repository it
//! touches; the shell (`workspace`) owns the children and asks this module.

use crate::json::Json;
use crate::scan::Id;
use std::collections::BTreeMap;

/// The ids of fleet-lsp's own requests to a child (`initialize`,
/// `shutdown`): a string the client never sends, whose replies stay inside.
const OWN_PREFIX: &str = "fleet-lsp:";

pub(crate) fn own_id(n: u64) -> Id {
    Id::Str(format!("{OWN_PREFIX}{n}"))
}

pub(crate) fn is_own(id: &Id) -> bool {
    matches!(id, Id::Str(s) if s.starts_with(OWN_PREFIX))
}

/// The id a child's request carries on the way to the client: the slot,
/// then the child's own id as JSON text, so two children's id 0 differ and
/// the client's answer maps back without a table.
pub(crate) fn client_facing_id(slot: u64, original: &Id) -> Id {
    Id::Str(format!("s{slot}:{}", original.to_json()))
}

/// The slot and the child's own id behind a client-facing id, or `None`
/// for an id fleet-lsp did not make.
pub(crate) fn from_client_facing(id: &Id) -> Option<(u64, Id)> {
    let Id::Str(s) = id else { return None };
    let (slot, original) = s.strip_prefix('s')?.split_once(':')?;
    let slot = slot.parse().ok()?;
    let original = match crate::json::parse(original).ok()? {
        Json::Int(n) => Id::Int(n),
        Json::Str(s) => Id::Str(s),
        Json::Num(_) => Id::Other(original.to_string()),
        _ => return None,
    };
    Some((slot, original))
}

/// A JSON value read as a request id (a `$/cancelRequest` target).
pub(crate) fn id_of(value: &Json) -> Option<Id> {
    match value {
        Json::Int(n) => Some(Id::Int(*n)),
        Json::Str(s) => Some(Id::Str(s.clone())),
        _ => None,
    }
}

/// `file://` and the path, every byte outside the unreserved set and `/`
/// percent-encoded: what `relay::file_uri_path` reads back.
pub(crate) fn file_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `msg` with its `id` replaced.
pub(crate) fn with_id(mut msg: Json, id: &Id) -> Json {
    let value = match id {
        Id::Int(n) => Json::Int(*n),
        Id::Str(s) => Json::Str(s.clone()),
        Id::Other(raw) => crate::json::parse(raw).unwrap_or(Json::Str(raw.clone())),
    };
    if let Json::Obj(m) = &mut msg {
        m.insert("id".into(), value);
    }
    msg
}

/// The document a client message names: `params.textDocument.uri`, or
/// `params.item.uri` (call hierarchy).
pub(crate) fn document_uri(msg: &Json) -> Option<&str> {
    let params = msg.get("params")?;
    params
        .get("textDocument")
        .or_else(|| params.get("item"))?
        .get("uri")?
        .as_str()
}

/// One document the client holds open: what a new child is told about it.
#[derive(Debug, Clone, PartialEq)]
struct Document {
    language_id: String,
    version: Json,
    text: String,
}

/// What a document notification did to the store.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    Stored,
    /// Not a document notification.
    Untouched,
    /// The store could not follow it; the reason is for the session log.
    Unfollowed(String),
}

/// The documents the client holds open, with their latest full text.
#[derive(Debug, Default)]
pub(crate) struct Documents {
    open: BTreeMap<String, Document>,
}

impl Documents {
    /// Folds a `didOpen`, `didChange` or `didClose` into the store. fleet-lsp
    /// advertises full sync, so a change carries the whole text.
    pub(crate) fn apply(&mut self, method: &str, msg: &Json) -> Applied {
        let Some(params) = msg.get("params") else {
            return Applied::Untouched;
        };
        let Some(uri) = document_uri(msg).map(str::to_string) else {
            return Applied::Untouched;
        };
        let doc = params.get("textDocument");
        match method {
            "textDocument/didOpen" => {
                let field = |k: &str| doc.and_then(|d| d.get(k));
                let (Some(language_id), Some(text)) = (
                    field("languageId").and_then(Json::as_str),
                    field("text").and_then(Json::as_str),
                ) else {
                    return Applied::Unfollowed(format!(
                        "didOpen of {uri} without languageId or text"
                    ));
                };
                self.open.insert(
                    uri,
                    Document {
                        language_id: language_id.to_string(),
                        version: field("version").cloned().unwrap_or(Json::Int(0)),
                        text: text.to_string(),
                    },
                );
                Applied::Stored
            }
            "textDocument/didChange" => {
                let full = params
                    .get("contentChanges")
                    .and_then(Json::as_arr)
                    .and_then(|changes| changes.last())
                    .filter(|change| change.get("range").is_none())
                    .and_then(|change| change.get("text"))
                    .and_then(Json::as_str);
                let Some(text) = full else {
                    return Applied::Unfollowed(format!("didChange of {uri} without a full text"));
                };
                let Some(stored) = self.open.get_mut(&uri) else {
                    return Applied::Unfollowed(format!("didChange of {uri}, which is not open"));
                };
                stored.text = text.to_string();
                if let Some(version) = doc.and_then(|d| d.get("version")) {
                    stored.version = version.clone();
                }
                Applied::Stored
            }
            "textDocument/didClose" => {
                self.open.remove(&uri);
                Applied::Stored
            }
            _ => Applied::Untouched,
        }
    }

    pub(crate) fn uris(&self) -> impl Iterator<Item = &str> {
        self.open.keys().map(String::as_str)
    }

    /// One `didOpen` per open document `belongs` keeps, in uri order: what
    /// a child that just started is told.
    pub(crate) fn replay(&self, belongs: impl Fn(&str) -> bool) -> Vec<Json> {
        self.open
            .iter()
            .filter(|(uri, _)| belongs(uri))
            .map(|(uri, d)| {
                let item = Json::obj()
                    .set("uri", uri.as_str())
                    .set("languageId", d.language_id.as_str())
                    .set("version", d.version.clone())
                    .set("text", d.text.as_str());
                Json::obj()
                    .set("jsonrpc", "2.0")
                    .set("method", "textDocument/didOpen")
                    .set("params", Json::obj().set("textDocument", item))
            })
            .collect()
    }
}

/// The roots in the order they were last used, most recent first.
#[derive(Debug, Default)]
pub(crate) struct Recent<K> {
    order: Vec<K>,
}

impl<K: PartialEq + Clone> Recent<K> {
    pub(crate) fn touch(&mut self, key: &K) {
        self.order.retain(|k| k != key);
        self.order.insert(0, key.clone());
    }

    pub(crate) fn latest(&self) -> Option<&K> {
        self.order.first()
    }

    /// The least recently used key `live` keeps, other than `keep`: the one
    /// to evict for `keep`.
    pub(crate) fn evictable(&self, live: impl Fn(&K) -> bool, keep: &K) -> Option<&K> {
        self.order.iter().rev().find(|k| *k != keep && live(k))
    }
}

/// The refusals workspace mode answers with, in the single-root form
/// `fleet-lsp: <lang>: <reason>; fix: <action>`, each naming what it is about.
pub(crate) mod refusal {
    pub(crate) fn no_repository(lang: &str, path: &str) -> String {
        format!("fleet-lsp: {lang}: {path} is in no git repository; fix: open a file inside a repository")
    }

    pub(crate) fn no_document(lang: &str) -> String {
        format!(
            "fleet-lsp: {lang}: no repository chosen yet; fix: open a file of the repository first"
        )
    }

    pub(crate) fn several(lang: &str, repo: &str, projects: &[String]) -> String {
        format!(
            "fleet-lsp: {lang}: several {lang} projects in {repo}; fix: open a file inside one of: {}",
            projects.join(", ")
        )
    }

    pub(crate) fn nothing(lang: &str, repo: &str) -> String {
        format!("fleet-lsp: {lang}: no {lang} project found in {repo}; fix: none — {repo} has none")
    }

    /// A single-root refusal (a pin, a version, a start failure), moved
    /// under the repository it is about.
    pub(crate) fn of_repository(lang: &str, repo: &str, single_root: &str) -> String {
        let prefix = format!("fleet-lsp: {lang}: ");
        let rest = single_root.strip_prefix(&prefix).unwrap_or(single_root);
        format!("{prefix}{repo}: {rest}; details: fleet-lsp doctor {repo}")
    }

    pub(crate) fn restarts_spent(lang: &str, repo: &str, starts: u32, log: &str) -> String {
        format!(
            "fleet-lsp: {lang}: {repo}: the server exited {starts} times; fix: none — read {log}; details: fleet-lsp doctor {repo}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn msg(text: &str) -> Json {
        parse(text).expect("json")
    }

    #[test]
    fn the_document_a_message_names() {
        let open = msg(
            r#"{"method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///a/x.py","languageId":"python","version":1,"text":"x"}}}"#,
        );
        assert_eq!(document_uri(&open), Some("file:///a/x.py"));
        let definition = msg(
            r#"{"id":1,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///b/y.rs"},"position":{"line":0,"character":0}}}"#,
        );
        assert_eq!(document_uri(&definition), Some("file:///b/y.rs"));
        let calls = msg(
            r#"{"id":2,"method":"callHierarchy/incomingCalls","params":{"item":{"uri":"file:///c/z.go","name":"f"}}}"#,
        );
        assert_eq!(document_uri(&calls), Some("file:///c/z.go"));
        let symbol = msg(r#"{"id":3,"method":"workspace/symbol","params":{"query":"f"}}"#);
        assert_eq!(document_uri(&symbol), None);
    }

    /// FALSIFY: drop the slot from `client_facing_id`.
    #[test]
    fn two_children_with_the_same_request_id_reach_the_client_apart_and_map_back() {
        let a = client_facing_id(1, &Id::Int(0));
        let b = client_facing_id(2, &Id::Int(0));
        assert_ne!(a, b);
        assert_eq!(from_client_facing(&a), Some((1, Id::Int(0))));
        assert_eq!(from_client_facing(&b), Some((2, Id::Int(0))));
        let s = client_facing_id(7, &Id::Str("cfg:1".into()));
        assert_eq!(from_client_facing(&s), Some((7, Id::Str("cfg:1".into()))));
        assert_eq!(from_client_facing(&Id::Int(4)), None);
        assert_eq!(from_client_facing(&Id::Str("fleet-lsp:3".into())), None);
    }

    #[test]
    fn a_file_uri_reads_back_to_its_path() {
        let path = std::path::Path::new("/Users/me/Developer/My Repo/é.rs");
        let uri = file_uri(path);
        assert_eq!(uri, "file:///Users/me/Developer/My%20Repo/%C3%A9.rs");
        assert_eq!(crate::relay::file_uri_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn own_ids_are_told_apart() {
        assert!(is_own(&own_id(3)));
        assert!(!is_own(&Id::Int(3)));
        assert!(!is_own(&Id::Str("s1:0".into())));
    }

    #[test]
    fn with_id_replaces_only_the_id() {
        let m = with_id(msg(r#"{"id":"s1:0","result":[1]}"#), &Id::Int(0));
        assert_eq!(m.to_string(), r#"{"id":0,"result":[1]}"#);
    }

    #[test]
    fn the_store_follows_open_change_close_and_replays_what_belongs() {
        let mut docs = Documents::default();
        let open = |uri: &str| {
            msg(&format!(
                r#"{{"params":{{"textDocument":{{"uri":"{uri}","languageId":"python","version":1,"text":"v1"}}}}}}"#
            ))
        };
        assert_eq!(
            docs.apply("textDocument/didOpen", &open("file:///a/x.py")),
            Applied::Stored
        );
        assert_eq!(
            docs.apply("textDocument/didOpen", &open("file:///b/y.py")),
            Applied::Stored
        );
        let change = msg(
            r#"{"params":{"textDocument":{"uri":"file:///a/x.py","version":2},"contentChanges":[{"text":"v2"}]}}"#,
        );
        assert_eq!(
            docs.apply("textDocument/didChange", &change),
            Applied::Stored
        );
        let replay = docs.replay(|uri| uri.starts_with("file:///a/"));
        assert_eq!(replay.len(), 1);
        let text = replay[0].to_string();
        assert!(
            text.contains(r#""text":"v2""#) && text.contains(r#""version":2"#),
            "{text}"
        );
        let close = msg(r#"{"params":{"textDocument":{"uri":"file:///a/x.py"}}}"#);
        assert_eq!(docs.apply("textDocument/didClose", &close), Applied::Stored);
        assert!(docs.replay(|uri| uri.starts_with("file:///a/")).is_empty());
        let ranged = msg(
            r#"{"params":{"textDocument":{"uri":"file:///b/y.py","version":2},"contentChanges":[{"range":{},"text":"x"}]}}"#,
        );
        assert!(matches!(
            docs.apply("textDocument/didChange", &ranged),
            Applied::Unfollowed(_)
        ));
        assert_eq!(docs.apply("textDocument/hover", &close), Applied::Untouched);
    }

    #[test]
    fn the_least_recent_live_root_is_evicted_never_the_one_asked_for() {
        let mut recent = Recent::default();
        for k in ["a", "b", "c"] {
            recent.touch(&k);
        }
        recent.touch(&"a");
        assert_eq!(recent.latest(), Some(&"a"));
        assert_eq!(recent.evictable(|_| true, &"d"), Some(&"b"));
        assert_eq!(recent.evictable(|k| *k != "b", &"d"), Some(&"c"));
        assert_eq!(recent.evictable(|k| *k == "a", &"a"), None);
    }

    #[test]
    fn refusals_name_the_repository_and_keep_the_single_root_form() {
        let r = refusal::of_repository(
            "python",
            "~/r",
            "fleet-lsp: python: no pinned pyright; fix: uv sync",
        );
        assert_eq!(
            r,
            "fleet-lsp: python: ~/r: no pinned pyright; fix: uv sync; details: fleet-lsp doctor ~/r"
        );
        assert!(refusal::several("rust", "~/r", &["a".into(), "b".into()])
            .ends_with("open a file inside one of: a, b"));
        assert!(refusal::no_repository("rust", "/tmp/x.rs")
            .starts_with("fleet-lsp: rust: /tmp/x.rs is in no git repository; fix:"));
    }
}
