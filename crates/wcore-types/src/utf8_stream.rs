//! Incremental UTF-8 decoder for byte streams whose chunk boundaries may fall
//! in the middle of a multi-byte codepoint.
//!
//! `reqwest::Response::bytes_stream()` (and any TCP/TLS-framed body) yields
//! arbitrary-sized byte chunks with no guarantee of landing on a UTF-8
//! codepoint boundary. Decoding each chunk independently with
//! `String::from_utf8_lossy` turns a codepoint split across two chunks into the
//! replacement character `U+FFFD` on BOTH halves — silently corrupting any
//! non-ASCII text (emoji, CJK, accented Latin, smart quotes) and, worse, the
//! streamed tool-call argument JSON assembled from those chunks.
//!
//! [`Utf8StreamDecoder`] fixes this by retaining the trailing incomplete bytes
//! of each chunk and prepending them to the next, so only genuinely invalid
//! byte sequences are lossily replaced. The retained tail is at most 3 bytes
//! (the longest possible prefix of an unfinished 4-byte codepoint), so the
//! decoder adds no unbounded buffering.

/// Stateful incremental UTF-8 decoder. Create one per stream, feed chunks via
/// [`push`](Self::push), and call [`finish`](Self::finish) at end-of-stream.
#[derive(Debug, Default)]
pub struct Utf8StreamDecoder {
    /// Trailing bytes of an incomplete codepoint carried to the next chunk.
    /// Bounded to at most 3 bytes by construction.
    tail: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub fn new() -> Self {
        Self { tail: Vec::new() }
    }

    /// Feed the next byte chunk. Returns the decoded text for every COMPLETE
    /// codepoint available so far; any trailing incomplete codepoint bytes are
    /// buffered for the next call. Genuinely invalid byte sequences are
    /// replaced with `U+FFFD` (matching `from_utf8_lossy` semantics) so the
    /// decoder always makes forward progress.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        if self.tail.is_empty() && chunk.is_empty() {
            return String::new();
        }
        let mut bytes = std::mem::take(&mut self.tail);
        bytes.extend_from_slice(chunk);

        let mut out = String::new();
        let mut input: &[u8] = &bytes;
        loop {
            match std::str::from_utf8(input) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    // SAFETY: `valid_up_to` bytes are confirmed valid UTF-8 by
                    // the `from_utf8` call above.
                    out.push_str(unsafe { std::str::from_utf8_unchecked(&input[..valid_up_to]) });
                    match e.error_len() {
                        // `None` => the bytes from `valid_up_to` to the end are an
                        // INCOMPLETE (but so-far-valid) codepoint prefix. Carry
                        // them to the next chunk instead of corrupting them.
                        None => {
                            self.tail = input[valid_up_to..].to_vec();
                            break;
                        }
                        // `Some(n)` => a genuine invalid sequence of `n` bytes.
                        // Emit one replacement char and continue past it.
                        Some(n) => {
                            out.push('\u{FFFD}');
                            input = &input[valid_up_to + n..];
                        }
                    }
                }
            }
        }
        out
    }

    /// Flush at end-of-stream. Any bytes still buffered are an incomplete
    /// codepoint at the truncated tail of the stream; emit a single replacement
    /// character for them, matching lossy semantics.
    pub fn finish(&mut self) -> String {
        if self.tail.is_empty() {
            String::new()
        } else {
            self.tail.clear();
            "\u{FFFD}".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_through_unchanged() {
        let mut d = Utf8StreamDecoder::new();
        assert_eq!(d.push(b"hello world"), "hello world");
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn multibyte_split_across_chunks_is_not_corrupted() {
        // "café" — 'é' is 0xC3 0xA9, split across two chunks.
        let full = "café".as_bytes();
        let split = full.len() - 1; // boundary inside the 'é'
        let mut d = Utf8StreamDecoder::new();
        let mut out = d.push(&full[..split]);
        out.push_str(&d.push(&full[split..]));
        out.push_str(&d.finish());
        assert_eq!(out, "café");
        assert!(!out.contains('\u{FFFD}'));
    }

    #[test]
    fn four_byte_emoji_split_every_boundary() {
        // U+1F600 = 0xF0 0x9F 0x98 0x80. Feed one byte at a time.
        let bytes = "😀".as_bytes();
        let mut d = Utf8StreamDecoder::new();
        let mut out = String::new();
        for b in bytes {
            out.push_str(&d.push(&[*b]));
        }
        out.push_str(&d.finish());
        assert_eq!(out, "😀");
    }

    #[test]
    fn genuinely_invalid_bytes_become_replacement() {
        let mut d = Utf8StreamDecoder::new();
        // 0xFF is never valid UTF-8.
        let out = d.push(&[b'a', 0xFF, b'b']);
        assert_eq!(out, "a\u{FFFD}b");
    }

    #[test]
    fn truncated_tail_flushes_as_replacement() {
        let mut d = Utf8StreamDecoder::new();
        // Start of 'é' (0xC3) with no continuation byte, then stream ends.
        assert_eq!(d.push(&[0xC3]), "");
        assert_eq!(d.finish(), "\u{FFFD}");
    }

    #[test]
    fn tail_never_exceeds_three_bytes() {
        let mut d = Utf8StreamDecoder::new();
        // Feed only the first byte of a 4-byte codepoint repeatedly is invalid;
        // feed a valid 3-byte prefix of a 4-byte sequence to exercise the tail.
        d.push(&[0xF0, 0x9F, 0x98]); // 3 of 4 bytes of 😀
        assert!(d.tail.len() <= 3);
    }
}
