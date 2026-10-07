//! The top-level scan of a JSON-RPC frame: its `id`, its `method`, and
//! whether it carries `result`/`error` — read without parsing or rewriting
//! the body, which is relayed byte for byte.
//!
//! The scanner walks bytes with a depth counter and string/escape state, so
//! an `"id"` inside `params`, a quote escaped inside a string, or `}{` inside
//! a document's text never matches. Only values at depth 1 under a top-level
//! key are captured, and only for the keys it reads. It is push-based
//! (`feed` any number of chunks, then `finish`) so a frame too large to
//! buffer can still be scanned while it is discarded.

/// A JSON-RPC id, decoded: two spellings of one string are one id
/// (`"abc"` and the same text with `a` for `a`), while `1` and `"1"`
/// stay distinct, as JSON-RPC says they are.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Id {
    Int(i64),
    Str(String),
    /// A number that is not an `i64` (fractional, huge): matched by its text.
    Other(String),
}

impl Id {
    /// The id as JSON text, for a reply fleet-lsp writes itself.
    pub(crate) fn to_json(&self) -> String {
        match self {
            Id::Int(n) => n.to_string(),
            Id::Other(raw) => raw.clone(),
            Id::Str(s) => {
                let mut out = String::new();
                crate::json::Json::Str(s.clone()).write(&mut out);
                out
            }
        }
    }
}

/// What the scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) id: Option<Id>,
    pub(crate) method: Option<String>,
    /// `result` or `error` present at the top level: a response.
    pub(crate) is_response: bool,
}

impl Scan {
    pub(crate) fn kind(&self) -> Kind {
        match (&self.method, &self.id) {
            (Some(_), Some(_)) => Kind::Request,
            (Some(_), None) => Kind::Notification,
            (None, Some(_)) if self.is_response => Kind::Response,
            _ => Kind::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Request,
    Notification,
    Response,
    /// Not a JSON-RPC message fleet-lsp understands; relayed untouched.
    Other,
}

/// Values longer than this are not captured (an id or a method name never is).
const CAPTURE_MAX: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// Inside the top-level object, waiting for a key (or `}`).
    Key,
    /// After a key, waiting for `:`.
    Colon,
    /// After `:`, inside the value (until `,` or `}` at depth 1).
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Id,
    Method,
    Ignored,
}

/// Push-based top-level scanner.
#[derive(Debug)]
pub(crate) struct TopScan {
    depth: usize,
    in_str: bool,
    esc: bool,
    expect: Expect,
    key: Vec<u8>,
    key_overflow: bool,
    field: Field,
    value: Vec<u8>,
    value_overflow: bool,
    out: Scan,
}

impl Default for TopScan {
    fn default() -> Self {
        TopScan {
            depth: 0,
            in_str: false,
            esc: false,
            expect: Expect::Key,
            key: Vec::new(),
            key_overflow: false,
            field: Field::Ignored,
            value: Vec::new(),
            value_overflow: false,
            out: Scan::default(),
        }
    }
}

impl TopScan {
    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            self.byte(b);
        }
    }

    pub(crate) fn finish(mut self) -> Scan {
        // A value still open at the end (truncated frame) is not trusted.
        if self.expect == Expect::Value && self.depth == 1 {
            self.end_value();
        }
        self.out
    }

    fn byte(&mut self, b: u8) {
        let capturing_value = self.depth >= 1 && self.expect == Expect::Value;
        let capturing_key = self.depth == 1 && self.expect == Expect::Key;
        if self.in_str {
            if capturing_key {
                self.push_key(b);
            } else if capturing_value {
                self.push_value(b);
            }
            if self.esc {
                self.esc = false;
            } else if b == b'\\' {
                self.esc = true;
            } else if b == b'"' {
                self.in_str = false;
                if capturing_key {
                    self.expect = Expect::Colon;
                }
            }
            return;
        }
        match b {
            b'"' => {
                self.in_str = true;
                if capturing_key {
                    self.key.clear();
                    self.key_overflow = false;
                    self.push_key(b);
                } else if capturing_value {
                    self.push_value(b);
                }
            }
            b'{' | b'[' => {
                if capturing_value {
                    self.push_value(b);
                }
                self.depth += 1;
                if self.depth == 1 {
                    self.expect = Expect::Key;
                }
            }
            b'}' | b']' => {
                if self.depth == 1 {
                    if self.expect == Expect::Value {
                        self.end_value();
                    }
                    self.depth = 0;
                    return;
                }
                if capturing_value {
                    self.push_value(b);
                }
                self.depth = self.depth.saturating_sub(1);
            }
            b':' if self.depth == 1 && self.expect == Expect::Colon => {
                self.field = match decode_string(&self.key).as_deref() {
                    _ if self.key_overflow => Field::Ignored,
                    Some("id") => Field::Id,
                    Some("method") => Field::Method,
                    Some("result") | Some("error") => {
                        self.out.is_response = true;
                        Field::Ignored
                    }
                    _ => Field::Ignored,
                };
                self.value.clear();
                self.value_overflow = false;
                self.expect = Expect::Value;
            }
            b',' if self.depth == 1 => {
                if self.expect == Expect::Value {
                    self.end_value();
                }
                self.expect = Expect::Key;
            }
            _ => {
                if capturing_value {
                    self.push_value(b);
                }
            }
        }
    }

    fn push_key(&mut self, b: u8) {
        if self.key.len() < 64 {
            self.key.push(b);
        } else {
            self.key_overflow = true;
        }
    }

    fn push_value(&mut self, b: u8) {
        if self.field == Field::Ignored {
            return;
        }
        if self.value.len() < CAPTURE_MAX {
            self.value.push(b);
        } else {
            self.value_overflow = true;
        }
    }

    fn end_value(&mut self) {
        let field = std::mem::replace(&mut self.field, Field::Ignored);
        if self.value_overflow {
            return;
        }
        let raw = trim(&self.value);
        match field {
            Field::Id => self.out.id = decode_id(raw),
            Field::Method => self.out.method = decode_string(raw),
            Field::Ignored => {}
        }
    }
}

/// Scans a whole body at once.
pub(crate) fn scan(body: &[u8]) -> Scan {
    let mut s = TopScan::default();
    s.feed(body);
    s.finish()
}

fn trim(raw: &[u8]) -> &[u8] {
    let ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let start = raw.iter().position(|b| !ws(b)).unwrap_or(raw.len());
    let end = raw.iter().rposition(|b| !ws(b)).map_or(start, |i| i + 1);
    &raw[start..end]
}

fn decode_id(raw: &[u8]) -> Option<Id> {
    match raw.first()? {
        b'"' => decode_string(raw).map(Id::Str),
        b'n' if raw == b"null" => None,
        b'-' | b'0'..=b'9' => {
            let text = std::str::from_utf8(raw).ok()?;
            Some(match text.parse::<i64>() {
                Ok(n) => Id::Int(n),
                Err(_) => Id::Other(text.to_string()),
            })
        }
        _ => None,
    }
}

/// Decodes a JSON string literal (with its quotes) to text, `None` if it is
/// not one or an escape is malformed.
pub(crate) fn decode_string(raw: &[u8]) -> Option<String> {
    let inner = raw.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    let text = std::str::from_utf8(inner).ok()?;
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hi = hex4(&mut chars)?;
                let cp = if (0xD800..0xDC00).contains(&hi) {
                    if chars.next()? != '\\' || chars.next()? != 'u' {
                        return None;
                    }
                    let lo = hex4(&mut chars)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return None;
                    }
                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                } else {
                    hi
                };
                out.push(char::from_u32(cp)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn hex4(chars: &mut std::str::Chars<'_>) -> Option<u32> {
    let mut v = 0;
    for _ in 0..4 {
        v = v * 16 + chars.next()?.to_digit(16)?;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_notification_response() {
        let r = scan(br#"{"jsonrpc":"2.0","id":7,"method":"textDocument/references","params":{}}"#);
        assert_eq!(r.id, Some(Id::Int(7)));
        assert_eq!(r.method.as_deref(), Some("textDocument/references"));
        assert_eq!(r.kind(), Kind::Request);
        let n = scan(br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
        assert_eq!(n.kind(), Kind::Notification);
        let s = scan(br#"{"jsonrpc":"2.0","id":"a","result":null}"#);
        assert_eq!(s.kind(), Kind::Response);
        assert_eq!(s.id, Some(Id::Str("a".into())));
    }

    #[test]
    fn method_inside_params_is_not_the_method() {
        let r =
            scan(br#"{"jsonrpc":"2.0","params":{"method":"evil","id":99},"method":"m","id":1}"#);
        assert_eq!(r.method.as_deref(), Some("m"));
        assert_eq!(r.id, Some(Id::Int(1)));
    }

    #[test]
    fn id_inside_a_string_with_escaped_quotes_is_ignored() {
        let body = br#"{"params":{"text":"x\" ,\"id\": 5, \"method\":\"no"},"method":"didOpen"}"#;
        let r = scan(body);
        assert_eq!(r.id, None);
        assert_eq!(r.method.as_deref(), Some("didOpen"));
        assert_eq!(r.kind(), Kind::Notification);
    }

    #[test]
    fn quote_written_as_unicode_escape_does_not_end_a_string() {
        // " is a quote; inside a string it must not end it.
        let body = br#"{"params":{"t":"a",\"id\":3"},"method":"n"}"#;
        let r = scan(body);
        assert_eq!(r.id, None);
        assert_eq!(r.method.as_deref(), Some("n"));
    }

    #[test]
    fn closing_then_opening_brace_in_text() {
        let body = br#"{"params":{"text":"}{\"id\":4}{"},"id":2,"method":"m"}"#;
        assert_eq!(scan(body).id, Some(Id::Int(2)));
    }

    #[test]
    fn two_spellings_of_one_string_id_are_one_id() {
        let a = scan(br#"{"id":"abc","result":1}"#).id;
        let b = scan(br#"{"id":"abc","result":1}"#).id;
        assert_eq!(a, b);
        assert_ne!(
            scan(br#"{"id":1,"result":1}"#).id,
            scan(br#"{"id":"1","result":1}"#).id
        );
    }

    #[test]
    fn escaped_key_spelling_still_matches() {
        let r = scan(br#"{"id":12,"method":"m"}"#);
        assert_eq!(r.id, Some(Id::Int(12)));
    }

    #[test]
    fn whitespace_and_order() {
        let r = scan(b"{ \"method\" : \"m\" ,\n \"id\" :\t-3 }");
        assert_eq!(r.id, Some(Id::Int(-3)));
        assert_eq!(r.method.as_deref(), Some("m"));
    }

    #[test]
    fn chunked_feed_equals_whole() {
        let body = br#"{"jsonrpc":"2.0","params":{"textDocument":{"text":"fn \"id\" {}"}},"id":"xy","method":"textDocument/didOpen"}"#;
        for split in 1..body.len() {
            let mut s = TopScan::default();
            s.feed(&body[..split]);
            s.feed(&body[split..]);
            assert_eq!(s.finish(), scan(body), "split at {split}");
        }
    }

    #[test]
    fn null_id_and_fractional_id() {
        assert_eq!(scan(br#"{"id":null,"error":{}}"#).id, None);
        assert_eq!(
            scan(br#"{"id":1.5,"result":1}"#).id,
            Some(Id::Other("1.5".into()))
        );
    }

    #[test]
    fn id_to_json_round_trips() {
        assert_eq!(Id::Int(4).to_json(), "4");
        assert_eq!(Id::Str("a\"b".into()).to_json(), r#""a\"b""#);
    }
}
