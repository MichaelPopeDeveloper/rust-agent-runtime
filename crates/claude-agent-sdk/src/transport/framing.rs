//! NDJSON line framing for the CLI's stdout/stderr.
//!
//! The CLI writes one JSON message per line, but a read returns an arbitrary
//! chunk: a line can span many reads, and one read can hold many lines. The
//! naive approach — scan the whole pending buffer for `\n` from the front and
//! `drain` each line off — is accidentally O(n²): every read re-scans the
//! growing prefix, and every `drain` memmoves the remainder down.
//!
//! [`LineFramer`] is O(total bytes): it keeps a `start` cursor and only scans
//! bytes it has not seen yet (`memchr`, which is SIMD-accelerated), never
//! copies a line out to yield it, and compacts the buffer at most once per
//! fully-drained chunk instead of once per line.

use serde_json::Value;

use crate::errors::{Error, Result};

/// When the drained buffer's capacity exceeds this, shrink it back down.
const SHRINK_ABOVE: usize = 1024 * 1024;
/// Capacity kept after shrinking.
const KEEP_CAPACITY: usize = 64 * 1024;

/// Reassembles `\n`-delimited lines from arbitrary byte chunks without
/// per-line copies or quadratic rescans.
pub(crate) struct LineFramer {
    buf: Vec<u8>,
    /// First byte not yet handed out as part of a line.
    start: usize,
    /// Next byte to search from for a newline (>= `start`).
    scan: usize,
    /// Per-message byte cap; a line (or unterminated tail) above it is fatal.
    max: usize,
}

impl LineFramer {
    pub(crate) fn new(max: usize) -> Self {
        LineFramer {
            buf: Vec::with_capacity(16 * 1024),
            start: 0,
            scan: 0,
            max,
        }
    }

    /// Append a freshly read chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Return the next complete line (newline stripped), or `None` when the
    /// buffer holds no more complete lines. Only the bytes added since the last
    /// call are scanned.
    pub(crate) fn next_line(&mut self) -> Option<&[u8]> {
        // Search only the unscanned region.
        let rel = memchr(b'\n', &self.buf[self.scan..])?;
        let nl = self.scan + rel;
        let line_start = self.start;
        // Advance cursors past this line (and its newline).
        self.start = nl + 1;
        self.scan = self.start;
        Some(&self.buf[line_start..nl])
    }

    /// Bytes buffered but not yet yielded as a complete line.
    pub(crate) fn pending_len(&self) -> usize {
        self.buf.len() - self.start
    }

    /// True if the unterminated tail has grown past the cap.
    pub(crate) fn overflowed(&self) -> bool {
        self.pending_len() > self.max
    }

    /// The current unterminated tail (for overflow reporting / final flush).
    pub(crate) fn tail(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    /// Discard the current tail (used after emitting an over-cap stderr line so
    /// the buffer doesn't grow without bound on a producer that never emits a
    /// newline).
    pub(crate) fn reset_tail(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.scan = 0;
    }

    /// Drop already-yielded bytes. Call once after draining a chunk's lines:
    /// resets to empty when fully consumed (the common case), otherwise shifts
    /// the live tail down at most once per chunk rather than once per line.
    pub(crate) fn compact(&mut self) {
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
            self.scan = 0;
            // A single huge message (a big tool result) can balloon the buffer;
            // don't pin that capacity for the rest of a long-lived session.
            if self.buf.capacity() > SHRINK_ABOVE {
                self.buf.shrink_to(KEEP_CAPACITY);
            }
        } else if self.start >= 64 * 1024 {
            self.buf.drain(..self.start);
            self.scan -= self.start;
            self.start = 0;
        }
    }
}

/// Decode one framed line into a JSON value.
///
/// Returns `None` for blank lines and non-JSON diagnostics (some CLI builds
/// print `[SandboxDebug] ...` to stdout), `Some(Err)` for a line that looks
/// like JSON but does not parse. Parses straight from the bytes — no
/// `from_utf8_lossy` and no intermediate `String`.
pub(crate) fn decode_frame(line: &[u8]) -> Option<Result<Value>> {
    let trimmed = trim_ascii(line);
    if trimmed.is_empty() {
        return None;
    }
    if trimmed[0] != b'{' {
        return None;
    }
    Some(
        serde_json::from_slice(trimmed)
            .map_err(|e| Error::json_decode(String::from_utf8_lossy(trimmed).into_owned(), e)),
    )
}

/// Trim ASCII whitespace from both ends of a byte slice (stable since 1.80 as
/// `[u8]::trim_ascii`, inlined here to keep the MSRV low and avoid surprises).
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Find the first `needle` in `haystack` using the `memchr` crate's
/// runtime-SIMD scan — far faster than a byte-at-a-time loop on the large
/// lines big tool results produce.
#[inline]
fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    ::memchr::memchr(needle, haystack)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_lines(chunks: &[&[u8]], max: usize) -> Vec<Vec<u8>> {
        let mut framer = LineFramer::new(max);
        let mut out = Vec::new();
        for chunk in chunks {
            framer.push(chunk);
            // Copy each borrowed line out so we can keep calling next_line.
            while let Some(line) = framer.next_line() {
                out.push(line.to_vec());
            }
            framer.compact();
        }
        out
    }

    #[test]
    fn frames_lines_across_chunk_boundaries() {
        // A line split across three chunks, then two lines in one chunk.
        let lines = collect_lines(&[b"{\"a\":", b"1}\n{\"b\"", b":2}\n{\"c\":3}\n"], 1024);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], b"{\"a\":1}");
        assert_eq!(lines[1], b"{\"b\":2}");
        assert_eq!(lines[2], b"{\"c\":3}");
    }

    #[test]
    fn handles_many_lines_in_one_chunk() {
        let mut data = Vec::new();
        for i in 0..1000 {
            data.extend_from_slice(format!("{{\"i\":{i}}}\n").as_bytes());
        }
        let lines = collect_lines(&[&data], 1024);
        assert_eq!(lines.len(), 1000);
        assert_eq!(lines[0], b"{\"i\":0}");
        assert_eq!(lines[999], b"{\"i\":999}");
    }

    #[test]
    fn decode_skips_blank_and_non_json() {
        assert!(decode_frame(b"   ").is_none());
        assert!(decode_frame(b"[SandboxDebug] starting").is_none());
        let value = decode_frame(b"  {\"x\": 1}\r\n").unwrap().unwrap();
        assert_eq!(value["x"], 1);
    }

    #[test]
    fn decode_reports_bad_json() {
        let err = decode_frame(b"{not json}").unwrap().unwrap_err();
        assert!(matches!(err, Error::JsonDecode { .. }));
    }

    #[test]
    fn overflow_detected_on_unterminated_tail() {
        let mut framer = LineFramer::new(8);
        framer.push(b"{\"aaaaaaaaaaaaaaaa\":1}"); // no newline, > 8 bytes
        assert!(framer.next_line().is_none());
        assert!(framer.overflowed());
    }

    #[test]
    fn compact_shrinks_after_a_huge_message() {
        let mut framer = LineFramer::new(32 * 1024 * 1024);
        let mut big = vec![b'{'; 4 * 1024 * 1024];
        big.extend_from_slice(b"\"k\":1}\n");
        framer.push(&big);
        // Not valid JSON, but framing only needs the newline.
        assert!(framer.next_line().is_some());
        assert!(framer.buf.capacity() > SHRINK_ABOVE);
        framer.compact();
        assert!(framer.buf.capacity() <= KEEP_CAPACITY.max(16 * 1024));
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn compact_resets_when_drained() {
        let mut framer = LineFramer::new(1024);
        framer.push(b"{\"a\":1}\n");
        assert!(framer.next_line().is_some());
        assert!(framer.next_line().is_none());
        framer.compact();
        assert_eq!(framer.pending_len(), 0);
        // Reuse after compaction still frames correctly.
        framer.push(b"{\"b\":2}\n");
        assert_eq!(framer.next_line().unwrap(), b"{\"b\":2}");
    }
}
