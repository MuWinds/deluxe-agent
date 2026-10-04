//! Per-stream decoder state, keyed by the host's stream id.
//!
//! The host owns the socket and the retry loop; the Component only accumulates
//! the decoder for each in-flight stream. A stream's state is created on its
//! first chunk and dropped when the decoder reports the stream finished, or when
//! the host abandons the attempt with `close-stream`.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use crate::codec::Decoder;

/// Every decoder the host is currently feeding.
pub fn streams() -> &'static Mutex<BTreeMap<String, Decoder>> {
    static STREAMS: OnceLock<Mutex<BTreeMap<String, Decoder>>> = OnceLock::new();
    STREAMS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The message used whenever the stream map's lock is poisoned.
pub fn poisoned() -> String {
    "LLM stream state is poisoned".to_string()
}
