//! Incremental SSE line splitting, shared by every protocol.
//!
//! The byte stream is split on `\n` *before* being decoded to UTF-8: decoding
//! each chunk as it arrives would split a multi-byte character that straddles a
//! chunk boundary and corrupt the text.

/// Buffers raw response bytes and yields complete, newline-terminated lines.
#[derive(Default)]
pub struct SseBuffer {
    buffer: Vec<u8>,
}

impl SseBuffer {
    /// Appends a chunk and returns every complete line it completed, with the
    /// trailing `\r\n` / `\n` removed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line);
            lines.push(line.trim_end_matches(['\r', '\n']).to_string());
        }
        lines
    }
}

/// The JSON payload of a `data:` line, or `None` for a comment, an `event:`
/// line, a blank separator, or an empty payload.
///
/// A provider that also emits `event:` lines repeats the event name inside the
/// data JSON's `type` field, so the reader keys off the payload alone.
pub fn data_payload(line: &str) -> Option<&str> {
    let payload = line.strip_prefix("data:")?.trim();
    (!payload.is_empty()).then_some(payload)
}
