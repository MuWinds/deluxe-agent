//! Protocol codecs: request shaping and stream decoding, one module per API.
//!
//! Nothing here touches the host ABI. Each module is a pure function of its
//! inputs and its own per-stream state, so the whole codec is unit-tested on the
//! host target while the Component build compiles the same code for `wasm32`.

pub mod anthropic;
pub mod canonical;
pub mod openai_chat;
pub mod openai_responses;
pub mod sse;

use crate::config::Provider;
use canonical::{AssistantTurn, CanonicalRequest, Event, HttpRequest};

/// The wire protocol a provider profile speaks.
///
/// Kept apart from the vendor identity, so several vendors that share a shape
/// (DeepSeek, Kimi, and GLM all speak chat completions) reuse one codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
}

impl Protocol {
    /// Parses the stored spelling, or `None` for an unknown protocol.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "openai-chat" => Some(Self::OpenAiChat),
            "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic-messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }
}

/// The events decoded from one chunk, and whether the stream has ended.
#[derive(Debug)]
pub struct PushResult {
    pub events: Vec<Event>,
    pub done: bool,
}

/// Per-stream decoder state, one variant per protocol.
pub enum Decoder {
    OpenAiChat(openai_chat::State),
    OpenAiResponses(openai_responses::State),
    Anthropic(anthropic::State),
}

impl Decoder {
    pub fn new(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAiChat => Self::OpenAiChat(openai_chat::State::default()),
            Protocol::OpenAiResponses => Self::OpenAiResponses(openai_responses::State::default()),
            Protocol::AnthropicMessages => Self::Anthropic(anthropic::State::default()),
        }
    }

    /// Feeds one response chunk and returns the decoded events.
    pub fn push(&mut self, chunk: &[u8]) -> Result<PushResult, String> {
        match self {
            Self::OpenAiChat(state) => state.push(chunk),
            Self::OpenAiResponses(state) => state.push(chunk),
            Self::Anthropic(state) => state.push(chunk),
        }
    }

    /// Consumes the decoder into the accumulated turn.
    pub fn turn(self) -> AssistantTurn {
        match self {
            Self::OpenAiChat(state) => state.turn(),
            Self::OpenAiResponses(state) => state.turn(),
            Self::Anthropic(state) => state.turn(),
        }
    }
}

/// Shapes one turn into an HTTP request for the profile's protocol.
///
/// `key` is already resolved: the caller owns the inline-key / `get-secret`
/// precedence, so no protocol module has to reach a host capability.
pub fn build_request(
    profile: &Provider,
    key: &str,
    request: &CanonicalRequest,
) -> Result<HttpRequest, String> {
    match Protocol::parse(&profile.protocol) {
        Some(Protocol::OpenAiChat) => openai_chat::build_request(profile, key, request),
        Some(Protocol::OpenAiResponses) => openai_responses::build_request(profile, key, request),
        Some(Protocol::AnthropicMessages) => anthropic::build_request(profile, key, request),
        None => Err(format!("Unknown provider protocol `{}`", profile.protocol)),
    }
}

/// Decodes a non-streaming response body into a turn.
pub fn parse_complete(protocol: Protocol, body: &[u8]) -> Result<AssistantTurn, String> {
    match protocol {
        Protocol::OpenAiChat => openai_chat::parse_complete(body),
        Protocol::OpenAiResponses => openai_responses::parse_complete(body),
        Protocol::AnthropicMessages => anthropic::parse_complete(body),
    }
}
