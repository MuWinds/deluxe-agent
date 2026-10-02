//! The OpenAI-compatible chat client.
//!
//! Two things here are easy to get wrong and are worth reading before editing:
//!
//! 1. Tool-call streaming. `delta.tool_calls` arrives as fragments keyed by
//!    `index`, and `function.arguments` is a *string* that must be concatenated
//!    across chunks — it is not valid JSON until the turn ends. Accumulating it
//!    into a `Value` per chunk silently produces a truncated call.
//! 2. Line decoding. The byte stream is split on `\n` *before* being decoded
//!    to UTF-8. Decoding each chunk as it arrives would split a multi-byte
//!    character that straddles a chunk boundary and corrupt the text.
//! 3. Reasoning. DeepSeek-R1-style models stream the chain of thought in
//!    `delta.reasoning_content`; other providers spell it `reasoning`. Both are
//!    surfaced as [`StreamFragment::Reasoning`] and must never be mixed into
//!    the answer text.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::attachments::ImageRef;
use crate::config::MAX_RETRY_COUNT;
use crate::error::{AgentError, Result};

const RETRY_DELAY_BASE: Duration = Duration::from_millis(250);
const RETRY_DELAY_MAX: Duration = Duration::from_secs(30);

/// How much reasoning effort to ask a reasoning model for.
///
/// The six strengths are hardcoded because pi-ai fixes the union upstream:
/// `off | minimal | low | medium | high | xhigh | max`, where `off` is not a
/// strength but the absence of one. That absence is what `None` means wherever
/// this type is carried — the request then sends no parameter at all — so only
/// the six strengths are modelled here. A provider that spells a level
/// differently maps it; it does not add a level.
///
/// Lives beside the wire format rather than in the config because it is chosen
/// per conversation, like pi's shift+tab indicator: every session carries its
/// own level, and a run sends whichever one the session holds. Contrast the
/// input modalities, which describe the *endpoint* and so belong in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    /// Spelled out because `rename_all = "lowercase"` alone would produce
    /// `xhigh` → `xhigh` only by accident of the variant name; the explicit
    /// rename keeps the wire spelling from drifting if the variant is renamed.
    #[serde(rename = "xhigh")]
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// Every strength, weakest first — the order a picker lists them in.
    pub const ALL: [ThinkingLevel; 6] = [
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    /// The value sent as `reasoning_effort` on the wire.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The label a picker shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Minimal => "极简",
            Self::Low => "低",
            Self::Medium => "中",
            Self::High => "高",
            Self::Xhigh => "极高",
            Self::Max => "最高",
        }
    }
}

/// One entry in the conversation sent to the model.
///
/// This mirrors the wire format rather than modelling it as a tagged enum,
/// because the three shapes (plain, assistant-with-tool-calls, tool-result)
/// differ only by which optional fields are set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    /// `None` is serialised as `null`, which is what the API expects on an
    /// assistant message that carries tool calls.
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// A user turn's body: text plus, optionally, the attached images.
///
/// Carried as a struct rather than a bare string so the GUI can hand the
/// composer's text and its attachments over in one value, and so history replay
/// round-trips the attachments — an image pinned to one prompt travels with
/// that prompt on every later turn.
#[derive(Debug, Clone, Default)]
pub struct UserTurn {
    pub text: String,
    pub images: Vec<ImageRef>,
}

/// A text-only turn, which is what most callers mean.
///
/// Every existing prompt is a bare string, so these keep `"hi".into()` working
/// at the call sites that predate attachments.
impl From<&str> for UserTurn {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for UserTurn {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

/// A message body: plain text, or an ordered list of parts.
///
/// Untagged on purpose. A text-only message serialises as a bare string exactly
/// as it always has, and only a message that carries an image becomes an array
/// — so nothing that never touches an image changes shape on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// One part of a multimodal message body.
///
/// `rename_all = "snake_case"` is load-bearing: without it `ImageUrl` would
/// serialise as `"ImageUrl"` rather than the `"image_url"` the API expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

impl MessageContent {
    /// The text this content contributes, ignoring any image parts.
    ///
    /// Never returns base64: the compaction summariser renders a transcript
    /// through here, and an inlined data URL would be megabytes of noise in a
    /// prompt that is meant to be short.
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    ContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// One non-streaming assistant turn, as the context summariser needs it.
pub type Completion = AssistantTurn;

impl Message {
    /// A user turn that may carry images.
    ///
    /// Text-only turns keep the bare-string shape that has always gone over the
    /// wire; only a turn with an image becomes a content-parts array. An image
    /// whose stored bytes can no longer be read back degrades to a text note
    /// rather than failing the run — the model loses the picture, not the turn.
    pub fn user_turn(turn: UserTurn) -> Self {
        if turn.images.is_empty() {
            return Self::user(turn.text);
        }

        let mut parts = Vec::with_capacity(turn.images.len() + 1);
        if !turn.text.is_empty() {
            parts.push(ContentPart::Text { text: turn.text });
        }
        for image in &turn.images {
            match image.data_url() {
                Ok(url) => parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrl { url },
                }),
                Err(error) => {
                    tracing::warn!(id = %image.id, %error, "a stored image could not be read back");
                    let label = image.name.as_deref().unwrap_or(image.id.as_str());
                    parts.push(ContentPart::Text {
                        text: format!("[image {label} is no longer readable]"),
                    });
                }
            }
        }

        Self {
            role: "user".into(),
            content: Some(MessageContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// A `system` message — the assembled prompt, sent once at the head of a
    /// request.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// A `user` message whose whole content is one block of text.
    ///
    /// For the text-only case; [`Message::user_with_images`] wraps an
    /// attachment into a multimodal parts array.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// An `assistant` message: what the model said, and any tool calls it made.
    ///
    /// Empty text and an empty call list both serialise as `null` rather than
    /// as `""` / `[]`, because a provider that round-trips this message rejects
    /// a present-but-empty field where an absent one is expected.
    pub fn assistant(content: String, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: if content.is_empty() {
                None
            } else {
                Some(MessageContent::Text(content))
            },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        }
    }

    /// A `tool` message: the result of one call, addressed back to it by id.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// A tool result that carries images alongside its text.
    ///
    /// The images ride inside the `tool` message's own content array, which is
    /// the shape the DeepSeek chat-completions adapter produces for a
    /// tool-result image block. A reference that cannot be read back degrades
    /// to a text note rather than failing the run: the model loses the picture,
    /// not the whole turn.
    pub fn tool_with_images(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        images: &[ImageRef],
    ) -> Self {
        let text = content.into();
        if images.is_empty() {
            return Self::tool(tool_call_id, text);
        }

        let mut parts = Vec::with_capacity(images.len() + 1);
        if !text.is_empty() {
            parts.push(ContentPart::Text { text });
        }
        for image in images {
            match image.data_url() {
                Ok(url) => parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrl { url },
                }),
                Err(error) => {
                    tracing::warn!(id = %image.id, %error, "a stored image could not be read back");
                    let label = image.name.as_deref().unwrap_or(image.id.as_str());
                    parts.push(ContentPart::Text {
                        text: format!("[image {label} is no longer readable]"),
                    });
                }
            }
        }

        Self {
            role: "tool".into(),
            content: Some(MessageContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// The message's text, ignoring any image parts. Empty when it has none.
    pub fn text(&self) -> String {
        self.content
            .as_ref()
            .map(MessageContent::as_text)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON text exactly as the model emitted it.
    ///
    /// Kept as a string rather than a parsed `Value` so that a malformed body
    /// can be reported back to the model as a tool error instead of failing the
    /// whole run. It is only parsed at the moment of execution.
    pub arguments: String,
}

/// The token counts a provider reports for one request.
///
/// Every field is optional on purpose: a provider that does not send usage —
/// or sends a partial object — must not masquerade as a real measurement of
/// zero, because the context-window manager acts on these figures and a fake
/// zero would read as "the conversation is empty". A count that is genuinely
/// zero cannot be told apart from a missing one, and does not need to be.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    /// DeepSeek reports the cache split at the top level, as a hit count and a
    /// miss count that together add up to `prompt_tokens`.
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u64>,
    /// OpenAI reports the same fact nested, as `prompt_tokens_details`.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// OpenAI's nested `prompt_tokens_details`.
///
/// A struct rather than a bare `u64` because the object carries other counters
/// this client does not read; only `cached_tokens` is needed to price the
/// cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

impl Usage {
    /// The measured size of the prompt, when the provider reported one.
    pub fn prompt_tokens(&self) -> Option<u64> {
        self.prompt_tokens
    }

    /// Prompt tokens the provider served from its cache, from whichever field
    /// it used.
    ///
    /// DeepSeek's top-level `prompt_cache_hit_tokens` is tried first, then
    /// OpenAI's nested `prompt_tokens_details.cached_tokens`. `None` means the
    /// provider reported no cache figure at all, which is not the same as a
    /// reported zero: a cold first request legitimately reports zero hits.
    pub fn cached_prompt_tokens(&self) -> Option<u64> {
        self.prompt_cache_hit_tokens.or_else(|| {
            self.prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens)
        })
    }

    /// The cached share of the prompt, in `0.0..=1.0`.
    ///
    /// `None` when the provider reported no prompt size, or no cache figure to
    /// divide by it — a rate with no denominator would be a lie.
    pub fn cache_hit_rate(&self) -> Option<f32> {
        let prompt = self.prompt_tokens()?;
        if prompt == 0 {
            return None;
        }
        let cached = self.cached_prompt_tokens()?;
        Some((cached as f32 / prompt as f32).clamp(0.0, 1.0))
    }
}

/// One streamed fragment of an assistant turn.
///
/// The chain of thought and the answer travel in separate delta fields and are
/// kept apart end to end: the UI folds the former away by default, so mixing
/// them here would put thinking into the answer where it cannot be folded.
#[derive(Debug, Clone, Copy)]
pub enum StreamFragment<'a> {
    /// The previous attempt failed and its streamed output should be discarded.
    Reset,
    /// The model's chain of thought, when it thinks out loud.
    Reasoning(&'a str),
    /// The part of the reply the user is meant to read.
    Content(&'a str),
}

/// The result of one assistant turn.
#[derive(Debug, Clone)]
pub struct AssistantTurn {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

/// Fragments of one tool call, reassembled across stream chunks.
#[derive(Debug, Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// Cloneable so a sub-agent can run on the same endpoint as its parent: the
/// `reqwest::Client` is an `Arc` internally, so a clone shares the connection
/// pool rather than opening a second one.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    /// `max_tokens` to send with every request, or `None` to leave the
    /// provider's own ceiling in force.
    max_output_tokens: Option<u32>,
    /// Additional attempts after failure, or `None` to retry until cancelled.
    retry_limit: Option<u32>,
}

impl LlmClient {
    /// Builds a client for one OpenAI-compatible `/chat/completions` endpoint.
    ///
    /// The URL is stored without a trailing slash so request paths join cleanly.
    /// No overall request timeout is set — a streamed turn runs for minutes and
    /// is bounded by cancellation instead — only a connect timeout, so a dead
    /// host fails fast. `retry_limit` counts additional attempts; `None` retries
    /// until cancellation. Returns [`AgentError::internal`] if the HTTP client
    /// itself cannot be built.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        max_output_tokens: Option<u32>,
        retry_limit: Option<u32>,
    ) -> Result<Self> {
        // No overall timeout: a streamed turn legitimately runs for minutes.
        // Cancellation is what bounds it, plus a connect timeout so a dead host
        // fails fast instead of hanging the run.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                AgentError::internal(format!("Failed to build HTTP client: {error}"))
            })?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key: api_key.into(),
            max_output_tokens,
            retry_limit: retry_limit.map(|count| count.min(MAX_RETRY_COUNT)),
        })
    }

    /// Streams one assistant turn, invoking `on_fragment` for each fragment of
    /// the answer and, when the provider thinks out loud, of the reasoning that
    /// precedes it.
    ///
    /// `on_fragment` is called from the streaming task; the caller is responsible
    /// for coalescing fragments before touching the UI.
    ///
    /// Failed attempts emit [`StreamFragment::Reset`] before retrying, so the
    /// caller can discard any fragments already shown from that attempt.
    ///
    /// `thinking` is the conversation's chosen reasoning effort, or `None` to
    /// leave the parameter out. It is passed per call rather than held on the
    /// client because one client serves every session, and each session carries
    /// its own level.
    pub async fn stream_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        thinking: Option<ThinkingLevel>,
        cancel: &CancellationToken,
        mut on_fragment: impl FnMut(StreamFragment<'_>),
    ) -> Result<AssistantTurn> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.turn_body(messages, tools, thinking);
        let mut retry = 0_u64;

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::cancelled());
            }

            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                outcome = self.stream_turn_once(&url, &body, cancel, &mut on_fragment) => outcome,
            };
            match outcome {
                Ok(turn) => return Ok(turn),
                Err(_) if cancel.is_cancelled() => return Err(AgentError::cancelled()),
                Err(error) => {
                    on_fragment(StreamFragment::Reset);
                    if self
                        .retry_limit
                        .is_some_and(|limit| retry >= u64::from(limit))
                    {
                        return Err(error);
                    }

                    tracing::warn!(
                        retry = retry.saturating_add(1),
                        retry_forever = self.retry_limit.is_none(),
                        %error,
                        "retrying failed model stream"
                    );
                    let delay = retry_delay(retry);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    retry = retry.saturating_add(1);
                }
            }
        }
    }

    async fn stream_turn_once(
        &self,
        url: &str,
        body: &Value,
        cancel: &CancellationToken,
        on_fragment: &mut impl FnMut(StreamFragment<'_>),
    ) -> Result<AssistantTurn> {
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|error| AgentError::llm(format!("Request to {url} failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(AgentError::llm(format!(
                "{url} returned {status}: {}",
                truncate(&detail, 2000)
            )));
        }

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut accumulators: BTreeMap<usize, ToolCallAcc> = BTreeMap::new();
        let mut usage = None;
        let mut finish_reason = None;
        let mut done = false;
        let mut saw_choice = false;

        while !done {
            if cancel.is_cancelled() {
                return Err(AgentError::cancelled());
            }

            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                chunk = stream.next() => chunk,
            };

            let Some(chunk) = chunk else { break };
            let chunk =
                chunk.map_err(|error| AgentError::llm(format!("Stream failed: {error}")))?;
            buffer.extend_from_slice(&chunk);

            // Split on newlines before decoding, so a multi-byte character
            // straddling two chunks is never decoded in halves.
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=newline).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\r', '\n']);

                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    done = true;
                    break;
                }

                let event: Value = serde_json::from_str(payload).map_err(|error| {
                    AgentError::llm(format!("Malformed SSE payload: {error} — {payload}"))
                })?;

                if let Some(error) = event.get("error") {
                    return Err(AgentError::llm(format!("Provider error: {error}")));
                }

                if let Some(parsed) = event
                    .get("usage")
                    .and_then(|value| serde_json::from_value::<Usage>(value.clone()).ok())
                {
                    usage = Some(parsed);
                }

                let Some(choice) = event.get("choices").and_then(|c| c.get(0)) else {
                    continue;
                };
                saw_choice = true;

                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    finish_reason = Some(reason.to_string());
                }

                let Some(delta) = choice.get("delta") else {
                    continue;
                };

                if let Some(fragment) = delta.get("content").and_then(Value::as_str) {
                    content.push_str(fragment);
                    on_fragment(StreamFragment::Content(fragment));
                }

                // DeepSeek-R1-style models stream the chain of thought in a
                // field of its own; other providers spell it `reasoning`. It is
                // forwarded rather than accumulated: the transcript builds the
                // text from the fragments, and the model loop never sends
                // reasoning back.
                let reasoning = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(Value::as_str);
                if let Some(fragment) = reasoning {
                    on_fragment(StreamFragment::Reasoning(fragment));
                }

                absorb_tool_calls(delta, &mut accumulators);
            }
        }

        if !done {
            return Err(AgentError::llm(format!(
                "Stream from {url} ended before [DONE]"
            )));
        }
        if !saw_choice {
            return Err(AgentError::llm(format!(
                "Stream from {url} completed without a choice"
            )));
        }

        let tool_calls = accumulators
            .into_values()
            .map(|acc| ToolCall {
                id: if acc.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4())
                } else {
                    acc.id
                },
                call_type: "function".into(),
                function: FunctionCall {
                    name: acc.name,
                    arguments: acc.arguments,
                },
            })
            .collect();

        Ok(AssistantTurn {
            content,
            tool_calls,
            usage,
            finish_reason,
        })
    }

    /// The request body for one streamed turn.
    ///
    /// `reasoning_effort` is added only when a level was chosen, and
    /// `max_tokens` only when a budget was configured: an endpoint that has
    /// never heard of a parameter must not receive one just because the
    /// feature exists, so unset means the key is absent rather than null.
    fn turn_body(
        &self,
        messages: &[Message],
        tools: &Value,
        thinking: Option<ThinkingLevel>,
    ) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "stream": true,
            // Asks the provider to emit a final chunk carrying token counts.
            "stream_options": { "include_usage": true },
        });
        if let Some(level) = thinking {
            body["reasoning_effort"] = json!(level.wire());
        }
        if let Some(tokens) = self.max_output_tokens {
            body["max_tokens"] = json!(tokens);
        }
        body
    }

    /// One non-streaming assistant turn: the whole answer in one request.
    ///
    /// Used by the context-window compactor, which asks the model to summarise
    /// the conversation. The reply is not streamed — nothing here reaches the
    /// transcript until the summary text does.
    ///
    /// `tools` is the same schema the conversation's own requests carry. It is
    /// sent so the request keeps the conversation's cached prefix; the body
    /// pins `tool_choice` to `none` so the model answers with the summary
    /// rather than a tool call.
    pub async fn complete_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: &CancellationToken,
    ) -> Result<Completion> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.completion_body(messages, tools);

        let mut retry = 0_u64;
        loop {
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                outcome = self.complete_turn_once(&url, &body) => outcome,
            };
            match outcome {
                Ok(turn) => return Ok(turn),
                Err(_) if cancel.is_cancelled() => return Err(AgentError::cancelled()),
                Err(error) => {
                    if self
                        .retry_limit
                        .is_some_and(|limit| retry >= u64::from(limit))
                    {
                        return Err(error);
                    }

                    tracing::warn!(
                        retry = retry.saturating_add(1),
                        retry_forever = self.retry_limit.is_none(),
                        %error,
                        "retrying failed model completion"
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                        _ = tokio::time::sleep(retry_delay(retry)) => {}
                    }
                    retry = retry.saturating_add(1);
                }
            }
        }
    }

    /// The request body for one non-streaming turn.
    ///
    /// Carries the conversation's `tools` so the prompt prefix — and therefore
    /// the provider's cache — matches the request that triggered compaction.
    /// `tool_choice: "none"` is what keeps the model from answering with a call
    /// now that tools are on the table.
    fn completion_body(&self, messages: &[Message], tools: &Value) -> Value {
        let mut body = json!({
            "model": &self.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "none",
        });
        if let Some(tokens) = self.max_output_tokens {
            body["max_tokens"] = json!(tokens);
        }
        body
    }

    async fn complete_turn_once(&self, url: &str, body: &Value) -> Result<Completion> {
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|error| AgentError::llm(format!("Request to {url} failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(AgentError::llm(format!(
                "{url} returned {status}: {}",
                truncate(&detail, 2000)
            )));
        }

        let payload: Value = response
            .json()
            .await
            .map_err(|error| AgentError::llm(format!("Malformed response from {url}: {error}")))?;

        if let Some(error) = payload.get("error") {
            return Err(AgentError::llm(format!("Provider error: {error}")));
        }

        let choice = payload
            .get("choices")
            .and_then(|choices| choices.get(0))
            .ok_or_else(|| AgentError::llm(format!("{url} returned no choices: {payload}")))?;

        let content = choice
            .pointer("/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let usage = payload
            .get("usage")
            .and_then(|value| serde_json::from_value::<Usage>(value.clone()).ok());

        Ok(Completion {
            content,
            tool_calls: Vec::new(),
            usage,
            finish_reason: choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// Folds one `delta` object into the per-index tool-call accumulators.
fn absorb_tool_calls(delta: &Value, accumulators: &mut BTreeMap<usize, ToolCallAcc>) {
    let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) else {
        return;
    };

    for call in tool_calls {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let acc = accumulators.entry(index).or_default();

        if let Some(id) = call.get("id").and_then(Value::as_str) {
            acc.id = id.to_string();
        }

        let Some(function) = call.get("function") else {
            continue;
        };

        if let Some(name) = function.get("name").and_then(Value::as_str) {
            if !name.is_empty() {
                if acc.name.is_empty() {
                    acc.name = name.to_string();
                } else if !acc.name.ends_with(name) {
                    // Either the name is fragmented across chunks (append), or
                    // the provider repeats the whole name every chunk (skip).
                    acc.name.push_str(name);
                }
            }
        }

        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            acc.arguments.push_str(arguments);
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn retry_delay(retry: u64) -> Duration {
    let multiplier = 1_u32 << retry.min(7) as u32;
    RETRY_DELAY_BASE
        .saturating_mul(multiplier)
        .min(RETRY_DELAY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the accumulator state from a sequence of raw SSE `data:` payloads,
    /// exactly as `stream_turn` would.
    fn accumulate(payloads: &[&str]) -> Vec<ToolCall> {
        let mut accumulators: BTreeMap<usize, ToolCallAcc> = BTreeMap::new();
        for payload in payloads {
            let event: Value = serde_json::from_str(payload).unwrap();
            let delta = event
                .get("choices")
                .and_then(|choices| choices.get(0))
                .and_then(|choice| choice.get("delta"))
                .expect("payload must carry a delta");
            absorb_tool_calls(delta, &mut accumulators);
        }
        accumulators
            .into_values()
            .map(|acc| ToolCall {
                id: acc.id,
                call_type: "function".into(),
                function: FunctionCall {
                    name: acc.name,
                    arguments: acc.arguments,
                },
            })
            .collect()
    }

    #[test]
    fn reassembles_arguments_split_across_chunks() {
        // This is the case that breaks a naive implementation: `arguments` is
        // not valid JSON until the last fragment lands.
        let calls = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]}}]}"#,
        ]);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.txt"}"#);
        let parsed: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(parsed["path"], "a.txt");
    }

    #[test]
    fn keeps_parallel_tool_calls_ordered_by_index() {
        let calls = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
        ]);

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[1].function.name, "list_dir");
    }

    #[test]
    fn tolerates_a_provider_that_repeats_the_function_name() {
        let calls = accumulate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"exec","arguments":"{\"comm"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"exec","arguments":"and\":\"ls\"}"}}]}}]}"#,
        ]);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn ignores_chunks_that_carry_no_tool_calls() {
        let calls = accumulate(&[
            r#"{"choices":[{"delta":{"content":"thinking…"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        assert!(calls.is_empty());
    }

    #[test]
    fn retry_delay_grows_exponentially_and_stops_at_thirty_seconds() {
        assert_eq!(retry_delay(0), Duration::from_millis(250));
        assert_eq!(retry_delay(4), Duration::from_secs(4));
        assert_eq!(retry_delay(7), Duration::from_secs(30));
        assert_eq!(retry_delay(100), Duration::from_secs(30));
    }

    #[test]
    fn an_assistant_message_with_tool_calls_serialises_content_as_null() {
        let message = Message::assistant(
            String::new(),
            vec![ToolCall {
                id: "a".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "list_dir".into(),
                    arguments: "{}".into(),
                },
            }],
        );
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "assistant");
        assert!(value["content"].is_null());
        assert_eq!(value["tool_calls"][0]["function"]["name"], "list_dir");
    }

    #[test]
    fn a_tool_message_carries_its_call_id() {
        let value = serde_json::to_value(Message::tool("call_1", "ok")).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_call_id"], "call_1");
        assert!(value.get("tool_calls").is_none());
    }

    #[test]
    fn a_text_only_message_still_serialises_as_a_bare_string() {
        let value = serde_json::to_value(Message::user("hello")).unwrap();
        assert_eq!(value["content"], "hello");
    }

    #[test]
    fn an_image_part_serialises_as_an_openai_image_url() {
        let message = Message {
            role: "tool".into(),
            content: Some(MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "seen".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,AAAA".into(),
                    },
                },
            ])),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
        };

        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "seen");
        assert_eq!(value["content"][1]["type"], "image_url");
        assert_eq!(
            value["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn as_text_never_returns_image_data() {
        let content = MessageContent::Parts(vec![
            ContentPart::Text {
                text: "hello".into(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,AAAA".into(),
                },
            },
        ]);
        assert_eq!(content.as_text(), "hello");
    }

    fn client() -> LlmClient {
        LlmClient::new("http://localhost/v1", "m", "k", None, Some(0)).expect("the client builds")
    }

    #[test]
    fn a_configured_output_budget_is_sent_as_max_tokens() {
        let client = LlmClient::new("http://localhost/v1", "m", "k", Some(8192), Some(0))
            .expect("the client builds");
        let body = client.turn_body(&[], &json!([]), None);
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn an_unset_output_budget_leaves_max_tokens_out_entirely() {
        // Absent, not null: a provider that has never heard of `max_tokens`
        // must not be handed one just because the feature exists.
        let body = client().turn_body(&[], &json!([]), None);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn a_chosen_thinking_level_is_sent_as_reasoning_effort() {
        let body = client().turn_body(&[], &json!([]), Some(ThinkingLevel::Xhigh));
        assert_eq!(body["reasoning_effort"], "xhigh");
    }

    #[test]
    fn no_thinking_level_leaves_the_parameter_out_entirely() {
        // Absent, not null: an endpoint that never saw `reasoning_effort` must
        // not be handed one by default.
        let body = client().turn_body(&[], &json!([]), None);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn every_thinking_level_carries_its_wire_spelling() {
        // A renamed variant or a stray rename attribute would silently change
        // the request, so the value on the wire is asserted directly.
        for level in ThinkingLevel::ALL {
            let body = client().turn_body(&[], &json!([]), Some(level));
            assert_eq!(body["reasoning_effort"], level.wire(), "{level:?}");
        }
    }

    #[test]
    fn deepseek_reports_cache_hits_at_the_top_level() {
        let usage: Usage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "total_tokens": 105,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20,
        }))
        .expect("the DeepSeek shape parses");

        assert_eq!(usage.cached_prompt_tokens(), Some(80));
        assert_eq!(usage.cache_hit_rate(), Some(0.8));
    }

    #[test]
    fn openai_reports_cache_hits_nested_under_prompt_tokens_details() {
        let usage: Usage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "total_tokens": 105,
            "prompt_tokens_details": { "cached_tokens": 25 },
        }))
        .expect("the OpenAI shape parses");

        assert_eq!(usage.cached_prompt_tokens(), Some(25));
        assert_eq!(usage.cache_hit_rate(), Some(0.25));
    }

    #[test]
    fn a_provider_that_reports_no_cache_figure_yields_no_rate() {
        // Absent must stay absent: a provider that never mentions caching must
        // not read as a 0% hit rate, which would look like a cache that is
        // broken rather than one that was never reported.
        let usage: Usage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
        }))
        .expect("a usage without cache fields parses");

        assert_eq!(usage.cached_prompt_tokens(), None);
        assert_eq!(usage.cache_hit_rate(), None);
    }

    #[test]
    fn a_zero_prompt_has_no_rate_to_report() {
        let usage = Usage {
            prompt_tokens: Some(0),
            prompt_cache_hit_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(usage.cache_hit_rate(), None);
    }

    #[test]
    fn the_completion_body_keeps_the_conversation_tools_without_allowing_calls() {
        // The summary request has to carry the conversation's tools so it shares
        // the cached prefix, and pin `tool_choice` to `none` so the model
        // answers with the brief instead of calling a tool.
        let tools = json!([{ "type": "function", "function": { "name": "read_file" } }]);
        let body = client().completion_body(&[Message::user("summarise")], &tools);
        assert_eq!(body["tools"], tools);
        assert_eq!(body["tool_choice"], "none");
        assert!(
            body.get("stream").is_none(),
            "the summary is not streamed: {}",
            body
        );
    }
}
