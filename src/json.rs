//! A small JSON reader and writer.
//!
//! Copied from aval-core (`crates/aval-core/src/json.rs` at aval 9214398),
//! owned here rather than depended on (change.dependency-bar). fleet-lsp
//! relays frames byte for byte; this module is used only where a whole value
//! is needed — the `initialize` it re-serialises, the small status/progress
//! /log frames the barriers read, and `doctor --json`. It must never be put
//! in the path of an ordinary frame: re-serialising re-sorts keys and
//! rewrites `1.0` as `1`.
//!
//! The reader is **strict**, because it also parses JSON-RPC traffic for
//! `aval mcp` and that arrives from software nobody here wrote. It rejects what
//! JSON rejects: an escape outside `" \ / b f n r t u`, an unescaped control
//! character, and a surrogate that is not half of a well-formed pair. Numbers
//! carry their integer value exactly (`Json::Int`) so a request id survives the
//! round trip; only a fractional literal goes through `f64`.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    /// An integer, kept exact.
    ///
    /// `Num` would round anything past 2^53, and a JSON-RPC response MUST carry
    /// back the same id its request did. "No client sends an id that large" is
    /// a guess about other people's software, not a contract this can keep.
    Int(i64),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Ordered, so emitted objects are byte-stable across runs.
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub(crate) fn obj() -> Json {
        Json::Obj(BTreeMap::new())
    }

    pub(crate) fn set(mut self, k: &str, v: impl Into<Json>) -> Json {
        if let Json::Obj(m) = &mut self {
            m.insert(k.to_string(), v.into());
        }
        self
    }

    /// Insert only when present, so absent fields stay absent rather than null.
    pub(crate) fn set_opt<T: Into<Json>>(self, k: &str, v: Option<T>) -> Json {
        match v {
            Some(x) => self.set(k, x),
            None => self,
        }
    }

    /// The object at `path` below this one, created (and any non-object in
    /// the way replaced) so a caller can insert into it. `self` must be an
    /// object; anything else is turned into an empty one first.
    pub(crate) fn obj_at(&mut self, path: &[&str]) -> &mut BTreeMap<String, Json> {
        if !matches!(self, Json::Obj(_)) {
            *self = Json::obj();
        }
        let mut cur = self;
        for key in path {
            let Json::Obj(m) = cur else {
                unreachable!("made an object above")
            };
            let next = m.entry((*key).to_string()).or_insert_with(Json::obj);
            if !matches!(next, Json::Obj(_)) {
                *next = Json::obj();
            }
            cur = next;
        }
        match cur {
            Json::Obj(m) => m,
            _ => unreachable!("made an object above"),
        }
    }

    pub(crate) fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(k),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(n) => {
                let _ = write!(out, "{}", n);
            }
            Json::Num(n) => {
                if n.fract() == 0.0 && n.is_finite() {
                    let _ = write!(out, "{}", *n as i64);
                } else {
                    let _ = write!(out, "{}", n);
                }
            }
            Json::Str(s) => escape(s, out),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write(&mut s);
        f.write_str(&s)
    }
}

impl From<&str> for Json {
    fn from(v: &str) -> Json {
        Json::Str(v.to_string())
    }
}
impl From<String> for Json {
    fn from(v: String) -> Json {
        Json::Str(v)
    }
}
impl From<bool> for Json {
    fn from(v: bool) -> Json {
        Json::Bool(v)
    }
}
impl From<i32> for Json {
    fn from(v: i32) -> Json {
        Json::Int(v as i64)
    }
}
impl From<i64> for Json {
    fn from(v: i64) -> Json {
        Json::Int(v)
    }
}
impl From<usize> for Json {
    /// Past `i64::MAX` this falls back to `Num`, which rounds. Nothing in this
    /// crate counts that high; the arm exists so the conversion is total.
    fn from(v: usize) -> Json {
        match i64::try_from(v) {
            Ok(n) => Json::Int(n),
            Err(_) => Json::Num(v as f64),
        }
    }
}
impl From<f64> for Json {
    /// A non-finite float is **not JSON**, and there is no spelling of it that
    /// is: `NaN` and `inf` would be written literally and rejected by every
    /// reader. `null` is the honest value — "there is no number here" — and it
    /// keeps a malformed line from reaching a client at all.
    fn from(v: f64) -> Json {
        if v.is_finite() {
            Json::Num(v)
        } else {
            Json::Null
        }
    }
}
impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Json {
        Json::Arr(v.into_iter().map(Into::into).collect())
    }
}

fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------- reader

pub(crate) fn parse(src: &str) -> Result<Json, String> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let v = value(&b, &mut i)?;
    ws(&b, &mut i);
    if i != b.len() {
        return Err(format!("trailing input at character {}", i));
    }
    Ok(v)
}

fn ws(b: &[char], i: &mut usize) {
    while *i < b.len() && b[*i].is_whitespace() {
        *i += 1;
    }
}

fn value(b: &[char], i: &mut usize) -> Result<Json, String> {
    ws(b, i);
    match b.get(*i) {
        None => Err("unexpected end of input".into()),
        Some('{') => object(b, i),
        Some('[') => array(b, i),
        Some('"') => string(b, i).map(Json::Str),
        Some('t') => lit(b, i, "true", Json::Bool(true)),
        Some('f') => lit(b, i, "false", Json::Bool(false)),
        Some('n') => lit(b, i, "null", Json::Null),
        Some(_) => number(b, i),
    }
}

fn lit(b: &[char], i: &mut usize, word: &str, v: Json) -> Result<Json, String> {
    if b[*i..].starts_with(&word.chars().collect::<Vec<_>>()[..]) {
        *i += word.len();
        Ok(v)
    } else {
        Err(format!("expected `{}` at character {}", word, i))
    }
}

fn number(b: &[char], i: &mut usize) -> Result<Json, String> {
    let start = *i;
    while *i < b.len() && (b[*i].is_ascii_digit() || "+-.eE".contains(b[*i])) {
        *i += 1;
    }
    let s: String = b[start..*i].iter().collect();
    // An integer literal keeps its exact value. Routing it through f64 first
    // would round a JSON-RPC id past 2^53 before anything could echo it back.
    if !s.contains(['.', 'e', 'E']) {
        if let Ok(n) = s.parse::<i64>() {
            return Ok(Json::Int(n));
        }
    }
    s.parse::<f64>()
        .map(Json::Num)
        .map_err(|_| format!("bad number `{}`", s))
}

/// Four hex digits at `at`, as a code unit.
fn hex4(b: &[char], at: usize) -> Result<u32, String> {
    let hex: String = b
        .get(at..at + 4)
        .ok_or("truncated `\\u` escape")?
        .iter()
        .collect();
    u32::from_str_radix(&hex, 16).map_err(|_| format!("bad `\\u` escape `{}`", hex))
}

/// Strict, because this also reads protocol traffic.
///
/// The earlier version ended in a catch-all that mapped an unknown escape to
/// itself and pushed any character at all, so `"\q"` read as `q` and a raw
/// control character passed silently — both invalid JSON, accepted. It also
/// decoded each `\u` alone, which rejects the *valid* surrogate pair JSON uses
/// to spell an astral code point, since neither half is a `char` on its own.
fn string(b: &[char], i: &mut usize) -> Result<String, String> {
    *i += 1; // opening quote
    let mut out = String::new();
    while *i < b.len() {
        match b[*i] {
            '"' => {
                *i += 1;
                return Ok(out);
            }
            '\\' => {
                *i += 1;
                let c = *b.get(*i).ok_or("unterminated escape")?;
                match c {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let hi = hex4(b, *i + 1)?;
                        *i += 4;
                        if (0xD800..0xDC00).contains(&hi) {
                            // A high surrogate is half a character. The other
                            // half must follow, as its own `\u`.
                            if b.get(*i + 1) != Some(&'\\') || b.get(*i + 2) != Some(&'u') {
                                return Err(format!("lone high surrogate `\\u{:04X}`", hi));
                            }
                            let lo = hex4(b, *i + 3)?;
                            if !(0xDC00..0xE000).contains(&lo) {
                                return Err(format!(
                                    "`\\u{:04X}` is a high surrogate but `\\u{:04X}` is not a low one",
                                    hi, lo
                                ));
                            }
                            *i += 6;
                            let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(c).ok_or("bad surrogate pair")?);
                        } else if (0xDC00..0xE000).contains(&hi) {
                            return Err(format!("lone low surrogate `\\u{:04X}`", hi));
                        } else {
                            out.push(char::from_u32(hi).ok_or("bad code point")?);
                        }
                    }
                    other => return Err(format!("invalid escape `\\{}`", other)),
                }
                *i += 1;
            }
            c if (c as u32) < 0x20 => {
                return Err(format!(
                    "unescaped control character U+{:04X} in a string",
                    c as u32
                ))
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    Err("unterminated string".into())
}

fn array(b: &[char], i: &mut usize) -> Result<Json, String> {
    *i += 1;
    let mut out = Vec::new();
    ws(b, i);
    if b.get(*i) == Some(&']') {
        *i += 1;
        return Ok(Json::Arr(out));
    }
    loop {
        out.push(value(b, i)?);
        ws(b, i);
        match b.get(*i) {
            Some(',') => *i += 1,
            Some(']') => {
                *i += 1;
                return Ok(Json::Arr(out));
            }
            _ => return Err(format!("expected `,` or `]` at character {}", i)),
        }
    }
}

fn object(b: &[char], i: &mut usize) -> Result<Json, String> {
    *i += 1;
    let mut m = BTreeMap::new();
    ws(b, i);
    if b.get(*i) == Some(&'}') {
        *i += 1;
        return Ok(Json::Obj(m));
    }
    loop {
        ws(b, i);
        if b.get(*i) != Some(&'"') {
            return Err(format!("expected a key at character {}", i));
        }
        let k = string(b, i)?;
        ws(b, i);
        if b.get(*i) != Some(&':') {
            return Err(format!("expected `:` at character {}", i));
        }
        *i += 1;
        m.insert(k, value(b, i)?);
        ws(b, i);
        match b.get(*i) {
            Some(',') => *i += 1,
            Some('}') => {
                *i += 1;
                return Ok(Json::Obj(m));
            }
            _ => return Err(format!("expected `,` or `}}` at character {}", i)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_stable_objects() {
        let j = Json::obj().set("b", 2).set("a", "x");
        assert_eq!(j.to_string(), r#"{"a":"x","b":2}"#);
    }

    #[test]
    fn absent_options_stay_absent() {
        let j = Json::obj()
            .set("a", 1)
            .set_opt("b", None::<String>)
            .set_opt("c", Some("y"));
        assert_eq!(j.to_string(), r#"{"a":1,"c":"y"}"#);
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(
            Json::Str("a\"b\\c\nd".into()).to_string(),
            r#""a\"b\\c\nd""#
        );
    }

    #[test]
    fn round_trips() {
        let src = r#"{"a":[1,2,{"b":true,"c":null}],"d":"x\ny"}"#;
        assert_eq!(parse(src).unwrap().to_string(), src);
    }

    #[test]
    fn reads_nested_fixtures() {
        let j = parse(r#" { "cases": [ {"name": "one", "n": 3} ] } "#).unwrap();
        let c = &j.get("cases").unwrap().as_arr().unwrap()[0];
        assert_eq!(c.get("name").unwrap().as_str(), Some("one"));
        assert_eq!(c.get("n"), Some(&Json::Int(3)));
    }

    #[test]
    fn rejects_trailing_input() {
        assert!(parse("{} {}").is_err());
    }

    // ------------------------------------------------------------ strictness
    //
    // Each of these parsed successfully before, into something the document
    // did not say. A reader that also handles protocol traffic cannot do that.

    #[test]
    fn rejects_an_invalid_escape() {
        let e = parse(r#""a\qb""#).unwrap_err();
        assert!(e.contains("invalid escape"), "{}", e);
        // The permitted set, all of it, still reads.
        assert_eq!(
            parse(r#""\"\\\/\b\f\n\r\t""#).unwrap(),
            Json::Str("\"\\/\u{8}\u{c}\n\r\t".into())
        );
    }

    #[test]
    fn rejects_an_unescaped_control_character() {
        let e = parse("\"a\u{1}b\"").unwrap_err();
        assert!(e.contains("control character"), "{}", e);
        assert!(e.contains("U+0001"), "{}", e);
        // Escaped, the same character is fine.
        assert_eq!(
            parse(r##""a\u0001b""##).unwrap(),
            Json::Str("a\u{1}b".into())
        );
    }

    #[test]
    fn reads_a_surrogate_pair_as_one_character() {
        // The only way JSON can spell U+1F600, and it used to be rejected.
        assert_eq!(
            parse(r##""\uD83D\uDE00""##).unwrap(),
            Json::Str("\u{1F600}".into())
        );
        // A raw astral character needs no escape at all, and still works.
        assert_eq!(
            parse("\"\u{1F600}\"").unwrap(),
            Json::Str("\u{1F600}".into())
        );
    }

    #[test]
    fn rejects_a_lone_surrogate() {
        assert!(parse(r#""\uD83D""#).unwrap_err().contains("lone high"));
        assert!(parse(r#""\uDE00""#).unwrap_err().contains("lone low"));
        assert!(parse(r#""\uD83Dx""#).unwrap_err().contains("lone high"));
        // A high surrogate followed by a \u that is not a low one.
        let e = parse(r##""\uD83D\u0041""##).unwrap_err();
        assert!(e.contains("not a low one"), "{}", e);
    }

    #[test]
    fn integers_keep_their_exact_value() {
        // 2^53 + 1, the first integer f64 cannot represent. A JSON-RPC id this
        // large has to come back unchanged.
        let src = r#"{"id":9007199254740993}"#;
        assert_eq!(parse(src).unwrap().to_string(), src);
        assert_eq!(parse("-9223372036854775808").unwrap(), Json::Int(i64::MIN));
        // A fractional literal is still f64, and says so.
        assert!(matches!(parse("1.5").unwrap(), Json::Num(_)));
        assert!(matches!(parse("1e3").unwrap(), Json::Num(_)));
    }

    #[test]
    fn integer_output_is_unchanged_by_the_int_variant() {
        // `--json` payloads are a byte-stable contract. `Int` must serialise
        // exactly as the integral `Num` it replaces did.
        assert_eq!(Json::from(0).to_string(), "0");
        assert_eq!(Json::from(-7).to_string(), "-7");
        assert_eq!(Json::from(42usize).to_string(), "42");
        assert_eq!(Json::Num(5.0).to_string(), Json::Int(5).to_string());
        let j = Json::obj().set("exit", 5).set("n", 0usize);
        assert_eq!(j.to_string(), r#"{"exit":5,"n":0}"#);
    }
}
