//! LSP base-protocol framing: `Content-Length: N\r\n\r\n` then N body bytes.
//!
//! A body is allocated once at its exact length, so no buffer doubles while
//! it grows. A body over the frame limit is never buffered: it is fed to the
//! top-level scanner in chunks and discarded, so the relay can still answer
//! its id (`Frame::Oversize`).

use crate::scan::{Scan, TopScan};
use std::io::{self, Read, Write};

/// Largest body the relay accepts, either direction (plan: 32 MiB).
pub(crate) const MAX_FRAME: usize = 32 << 20;
/// A header block larger than this is not LSP.
const MAX_HEADER: usize = 8 << 10;
const CHUNK: usize = 64 << 10;

#[derive(Debug)]
pub(crate) enum Frame {
    Body(Vec<u8>),
    /// A body over `MAX_FRAME`: its length and the top-level scan of it.
    Oversize {
        len: usize,
        scan: Scan,
    },
}

/// Reads frames from a byte stream.
#[derive(Debug)]
pub(crate) struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
    max: usize,
}

impl<R: Read> FrameReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self::with_max(inner, MAX_FRAME)
    }

    pub(crate) fn with_max(inner: R, max: usize) -> Self {
        FrameReader {
            inner,
            buf: Vec::new(),
            max,
        }
    }

    /// The next frame; `Ok(None)` on a clean end of stream between frames.
    pub(crate) fn next_frame(&mut self) -> io::Result<Option<Frame>> {
        let len = match self.read_header()? {
            Some(n) => n,
            None => return Ok(None),
        };
        if len > self.max {
            return self.discard(len).map(Some);
        }
        let mut body = Vec::with_capacity(len);
        let take = len.min(self.buf.len());
        body.extend_from_slice(&self.buf[..take]);
        self.buf.drain(..take);
        if body.len() < len {
            body.resize(len, 0);
            let have = take;
            self.inner
                .read_exact(&mut body[have..])
                .map_err(eof_mid_frame)?;
        }
        Ok(Some(Frame::Body(body)))
    }

    fn read_header(&mut self) -> io::Result<Option<usize>> {
        loop {
            if let Some(end) = find(&self.buf, b"\r\n\r\n") {
                let head = std::str::from_utf8(&self.buf[..end])
                    .map_err(|_| bad("header is not UTF-8"))?
                    .to_string();
                self.buf.drain(..end + 4);
                return content_length(&head).map(Some);
            }
            if self.buf.len() > MAX_HEADER {
                return Err(bad("header block too large"));
            }
            let mut chunk = [0u8; 4096];
            let n = self.inner.read(&mut chunk)?;
            if n == 0 {
                return if self.buf.iter().all(|b| b.is_ascii_whitespace()) {
                    Ok(None)
                } else {
                    Err(eof_mid_frame(io::ErrorKind::UnexpectedEof.into()))
                };
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn discard(&mut self, len: usize) -> io::Result<Frame> {
        let mut scan = TopScan::default();
        let take = len.min(self.buf.len());
        scan.feed(&self.buf[..take]);
        self.buf.drain(..take);
        let mut left = len - take;
        let mut chunk = vec![0u8; CHUNK];
        while left > 0 {
            let want = left.min(CHUNK);
            self.inner
                .read_exact(&mut chunk[..want])
                .map_err(eof_mid_frame)?;
            scan.feed(&chunk[..want]);
            left -= want;
        }
        Ok(Frame::Oversize {
            len,
            scan: scan.finish(),
        })
    }
}

/// Writes one frame (header + body) and flushes.
pub(crate) fn write_frame<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()
}

fn content_length(head: &str) -> io::Result<usize> {
    for line in head.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse().map_err(|_| bad("bad Content-Length"));
            }
        }
    }
    Err(bad("missing Content-Length"))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn eof_mid_frame(e: io::Error) -> io::Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        io::Error::new(io::ErrorKind::UnexpectedEof, "stream ended inside a frame")
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Id;

    /// A reader that hands out at most `step` bytes per read.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }
    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(out.len()).min(self.data.len());
            out[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn framed(bodies: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for b in bodies {
            write_frame(&mut out, b).unwrap();
        }
        out
    }

    fn bodies(stream: &[u8], step: usize, max: usize) -> Vec<Frame> {
        let mut r = FrameReader::with_max(Trickle { data: stream, step }, max);
        let mut out = Vec::new();
        while let Some(f) = r.next_frame().unwrap() {
            out.push(f);
        }
        out
    }

    #[test]
    fn two_messages_in_one_read_and_split_headers() {
        let s = framed(&[br#"{"id":1}"#, br#"{"id":2}"#]);
        for step in [1, 3, 7, s.len()] {
            let got = bodies(&s, step, MAX_FRAME);
            assert_eq!(got.len(), 2, "step {step}");
            assert!(matches!(&got[1], Frame::Body(b) if b == br#"{"id":2}"#));
        }
    }

    #[test]
    fn body_cut_at_a_read_boundary() {
        let body = vec![b'x'; 70_000];
        let s = framed(&[&body]);
        let got = bodies(&s, 65_536, MAX_FRAME);
        assert!(matches!(&got[0], Frame::Body(b) if b.len() == 70_000));
    }

    #[test]
    fn header_case_and_extra_headers() {
        let s = b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{}";
        let got = bodies(s, 5, MAX_FRAME);
        assert!(matches!(&got[0], Frame::Body(b) if b == b"{}"));
    }

    #[test]
    fn limit_passes_and_one_over_is_oversize_with_its_id() {
        let max = 64;
        let mut at = br#"{"id":9,"method":"m","params":""#.to_vec();
        at.resize(max - 2, b'a');
        at.extend_from_slice(b"\"}");
        assert_eq!(at.len(), max);
        let mut over = at.clone();
        over.insert(over.len() - 2, b'a');
        let got = bodies(&framed(&[&at, &over, b"{}"]), 13, max);
        assert!(matches!(&got[0], Frame::Body(b) if b.len() == max));
        match &got[1] {
            Frame::Oversize { len, scan } => {
                assert_eq!(*len, max + 1);
                assert_eq!(scan.id, Some(Id::Int(9)));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&got[2], Frame::Body(b) if b == b"{}"),
            "stream stays in sync"
        );
    }

    #[test]
    fn id_after_large_params_in_an_oversize_request() {
        let mut body = br#"{"jsonrpc":"2.0","params":{"text":""#.to_vec();
        body.extend(std::iter::repeat(b'z').take(200_000));
        body.extend_from_slice(br#""},"id":"late","method":"textDocument/didChange"}"#);
        let got = bodies(&framed(&[&body]), 65_536, 1024);
        match &got[0] {
            Frame::Oversize { scan, .. } => assert_eq!(scan.id, Some(Id::Str("late".into()))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn eof_inside_a_frame_is_an_error() {
        let s = b"Content-Length: 10\r\n\r\n{\"a\"";
        let mut r = FrameReader::new(Trickle { data: s, step: 4 });
        assert!(r.next_frame().is_err());
    }

    #[test]
    fn clean_eof_between_frames() {
        let mut r = FrameReader::new(Trickle { data: b"", step: 4 });
        assert!(r.next_frame().unwrap().is_none());
    }
}
