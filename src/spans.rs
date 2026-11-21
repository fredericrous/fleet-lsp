//! Byte spans of JSON values, found without building a tree: the members of
//! an object and the elements of an array, each as the exact range of its
//! text. Used where a frame is too large to parse whole and only a part of
//! it is rewritten (`core::cap_published`). A span is checked only for its
//! shape (strings closed, brackets balanced), not for full JSON validity:
//! callers parse the small parts they read.

use std::ops::Range;

/// The first index at or after `i` that is not JSON whitespace.
pub(crate) fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// The end (exclusive) of the string starting at `b[i] == '"'`.
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// The end (exclusive) of the value starting at `b[i]`.
pub(crate) fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// The `(key, value)` spans of the object starting at `b[i] == '{'`; a key
/// span includes its quotes.
pub(crate) fn members(b: &[u8], i: usize) -> Option<Vec<(Range<usize>, Range<usize>)>> {
    if b.get(i) != Some(&b'{') {
        return None;
    }
    let mut out = Vec::new();
    let mut j = ws(b, i + 1);
    if b.get(j) == Some(&b'}') {
        return Some(out);
    }
    loop {
        if b.get(j) != Some(&b'"') {
            return None;
        }
        let key = j..string_end(b, j)?;
        j = ws(b, key.end);
        if b.get(j) != Some(&b':') {
            return None;
        }
        let start = ws(b, j + 1);
        let value = start..value_end(b, start)?;
        out.push((key, value.clone()));
        j = ws(b, value.end);
        match b.get(j)? {
            b',' => j = ws(b, j + 1),
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

/// The value span of member `name` among `members`, matching the decoded key
/// (two spellings of one key are one key).
pub(crate) fn member(
    b: &[u8],
    members: &[(Range<usize>, Range<usize>)],
    name: &str,
) -> Option<Range<usize>> {
    members
        .iter()
        .find(|(k, _)| crate::scan::decode_string(&b[k.clone()]).as_deref() == Some(name))
        .map(|(_, v)| v.clone())
}

/// The element spans of the array starting at `b[i] == '['`.
pub(crate) fn elements(b: &[u8], i: usize) -> Option<Vec<Range<usize>>> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut out = Vec::new();
    let mut j = ws(b, i + 1);
    if b.get(j) == Some(&b']') {
        return Some(out);
    }
    loop {
        let e = j..value_end(b, j)?;
        out.push(e.clone());
        j = ws(b, e.end);
        match b.get(j)? {
            b',' => j = ws(b, j + 1),
            b']' => return Some(out),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<'a>(b: &'a [u8], r: &Range<usize>) -> &'a str {
        std::str::from_utf8(&b[r.clone()]).unwrap()
    }

    #[test]
    fn members_and_elements_are_exact_spans() {
        let b = br#" {"a" : [1, {"x":"]}\"["}, "s"] , "b":true,"c":null}"#;
        let m = members(b, ws(b, 0)).unwrap();
        assert_eq!(m.len(), 3);
        let a = member(b, &m, "a").unwrap();
        assert_eq!(text(b, &a), r#"[1, {"x":"]}\"["}, "s"]"#);
        let e = elements(b, a.start).unwrap();
        let got: Vec<&str> = e.iter().map(|r| text(b, r)).collect();
        assert_eq!(got, ["1", r#"{"x":"]}\"["}"#, r#""s""#]);
        assert_eq!(text(b, &member(b, &m, "b").unwrap()), "true");
    }

    #[test]
    fn an_escaped_key_spelling_matches() {
        let b = br#"{"a":[]}"#;
        let m = members(b, 0).unwrap();
        assert!(member(b, &m, "a").is_some());
    }

    #[test]
    fn empty_and_broken_shapes() {
        assert_eq!(members(b"{ }", 0).unwrap().len(), 0);
        assert_eq!(elements(b"[ ]", 0).unwrap().len(), 0);
        assert!(members(br#"{"a":1"#, 0).is_none());
        assert!(elements(br#"[1,"x]"#, 0).is_none());
        assert!(members(b"[]", 0).is_none());
    }
}
