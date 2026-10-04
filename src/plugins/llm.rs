//! The Wasmtime model provider adapter.
//!
//! The Component owns the wire protocol — how a request is shaped and how a
//! response stream is decoded — while this adapter owns the socket: it sends
//! the request the Component built, pumps each response chunk back into the
//! Component, and forwards the fragments it decodes to the run's sink. The
//! split exists because a Component actor is strictly serialized with no
//! guest-to-host callback, so a provider that held the socket itself would block
//! the actor for the whole turn.
//!
//! Retry, backoff, and cancellation live here too, exactly as they did on the
//! old native client: a failed attempt asks the Component to drop its partial
//! stream, the sink is told to discard what it showed, and the request is sent
//! again.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::{AgentError, Result};
use crate::harness::ports::{LlmStreamEvent, LlmStreamSink};
use crate::llm::{AssistantTurn, Message, ThinkingLevel};
use crate::plugins::capabilities::CapabilityHub;
use crate::plugins::wasm_runtime::{ComponentActor, Operation};
use crate::plugins::{self, PluginCatalogue};

const RETRY_DELAY_BASE: Duration = Duration::from_millis(250);
const RETRY_DELAY_MAX: Duration = Duration::from_secs(30);

/// The plugin id the bundled model provider installs under.
pub const LLM_PROVIDER_PLUGIN_ID: &str = "llm-provider@deluxe-defaults";

/// The HTTP request the Component shapes for the host to send.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HttpRequest {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    body: String,
}

/// One decoded stream chunk: the fragments to forward, whether the stream is
/// over, and — only when it is — the accumulated turn.
#[derive(Debug, Clone, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    events: Vec<StreamEvent>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    turn: Option<AssistantTurn>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum StreamEvent {
    Reasoning { text: String },
    Content { text: String },
}

/// Drives one Component instance as the host's model provider.
///
/// This is the only provider type: a model provider is always a Wasmtime
/// Component, so there is no separate trait and adapter — the Component owns
/// the wire protocol while this type owns the socket.
pub struct LlmProvider {
    actor: Arc<ComponentActor>,
    http: reqwest::Client,
    /// Additional attempts after failure, or `None` to retry until cancelled.
    retry_limit: Option<u32>,
}

impl LlmProvider {
    /// Builds a provider over a loaded Component actor.
    ///
    /// No overall request timeout is set — a streamed turn legitimately runs for
    /// minutes and is bounded by cancellation instead — only a connect timeout,
    /// so a dead host fails fast.
    pub fn new(actor: Arc<ComponentActor>, retry_limit: Option<u32>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                AgentError::internal(format!("Failed to build HTTP client: {error}"))
            })?;
        Ok(Self {
            actor,
            http,
            retry_limit,
        })
    }

    /// Asks the Component to shape the canonical request into an HTTP request.
    async fn shape_request(
        &self,
        request: &Value,
        cancel: &CancellationToken,
    ) -> Result<HttpRequest> {
        let json = serde_json::to_string(request).map_err(|error| {
            AgentError::internal(format!("Canonical request is not serializable: {error}"))
        })?;
        let output = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentError::cancelled()),
            output = self.actor.call(Operation::LlmBuildRequest(json)) => output?,
        };
        serde_json::from_str(&output).map_err(|error| {
            AgentError::llm(format!(
                "The model provider returned an invalid request: {error}"
            ))
        })
    }

    /// Sends one request and returns its streaming response.
    async fn send(
        &self,
        request: &HttpRequest,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
            AgentError::llm(format!("Unsupported HTTP method `{}`", request.method))
        })?;
        let mut builder = self.http.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        builder = builder.body(request.body.clone());

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentError::cancelled()),
            response = builder.send() => response.map_err(|error| {
                AgentError::llm(format!("Request to {} failed: {error}", request.url))
            })?,
        };
        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(AgentError::llm(format!(
                "{} returned {status}: {}",
                request.url,
                truncate(&detail, 2000)
            )));
        }
        Ok(response)
    }

    /// Streams one attempt and forwards its fragments to `sink`.
    ///
    /// The stream id is unique per attempt and always released, so a failed
    /// attempt's partial decoder state cannot leak into the retry.
    async fn stream_once(
        &self,
        request: &HttpRequest,
        cancel: &CancellationToken,
        sink: &mut dyn LlmStreamSink,
    ) -> Result<AssistantTurn> {
        let stream_id = uuid::Uuid::new_v4().to_string();
        let result = self.stream_inner(&stream_id, request, cancel, sink).await;
        // Best-effort: the guest drops the state at `done` too, and a close on a
        // missing stream is a no-op.
        let _ = self.actor.call(Operation::LlmCloseStream(stream_id)).await;
        result
    }

    async fn stream_inner(
        &self,
        stream_id: &str,
        request: &HttpRequest,
        cancel: &CancellationToken,
        sink: &mut dyn LlmStreamSink,
    ) -> Result<AssistantTurn> {
        let response = self.send(request, cancel).await?;
        let mut stream = response.bytes_stream();
        let mut turn = None;

        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let chunk =
                chunk.map_err(|error| AgentError::llm(format!("Stream failed: {error}")))?;
            if chunk.is_empty() {
                continue;
            }
            let decoded = self
                .actor
                .call(Operation::LlmParseStream {
                    stream_id: stream_id.to_string(),
                    chunk: chunk.to_vec(),
                })
                .await?;
            let decoded: StreamChunk = serde_json::from_str(&decoded).map_err(|error| {
                AgentError::llm(format!(
                    "The model provider returned an invalid stream chunk: {error}"
                ))
            })?;
            for event in decoded.events {
                sink.push(match event {
                    StreamEvent::Reasoning { text } => LlmStreamEvent::Reasoning(text),
                    StreamEvent::Content { text } => LlmStreamEvent::Content(text),
                });
            }
            if decoded.done {
                turn = decoded.turn;
                break;
            }
        }

        turn.ok_or_else(|| {
            AgentError::llm(format!(
                "The stream from {} ended before the provider finished",
                request.url
            ))
        })
    }

    /// Runs one non-streaming attempt.
    async fn complete_once(
        &self,
        request: &HttpRequest,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn> {
        let response = self.send(request, cancel).await?;
        let body = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentError::cancelled()),
            body = response.bytes() => body
                .map_err(|error| AgentError::llm(format!("Response body failed: {error}")))?,
        };
        let decoded = self
            .actor
            .call(Operation::LlmParseComplete(body.to_vec()))
            .await?;
        serde_json::from_str(&decoded).map_err(|error| {
            AgentError::llm(format!(
                "The model provider returned an invalid completion: {error}"
            ))
        })
    }

    /// Waits out the backoff before the next attempt, or returns early when the
    /// run is cancelled.
    async fn backoff(&self, retry: u64, cancel: &CancellationToken) -> Result<()> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AgentError::cancelled()),
            _ = tokio::time::sleep(retry_delay(retry)) => Ok(()),
        }
    }

    /// Whether another attempt is allowed after `retry` failures.
    fn may_retry(&self, retry: u64) -> bool {
        !self
            .retry_limit
            .is_some_and(|limit| retry >= u64::from(limit))
    }
}

impl LlmProvider {
    /// Streams one assistant turn and forwards provider fragments to `sink`.
    pub async fn stream_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        thinking: Option<ThinkingLevel>,
        cancel: &CancellationToken,
        sink: &mut dyn LlmStreamSink,
    ) -> Result<AssistantTurn> {
        let request = canonical_request(messages, tools, thinking, true, None);
        let http = self.shape_request(&request, cancel).await?;
        let mut retry = 0_u64;

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::cancelled());
            }
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                outcome = self.stream_once(&http, cancel, sink) => outcome,
            };
            match outcome {
                Ok(turn) => return Ok(turn),
                Err(_) if cancel.is_cancelled() => return Err(AgentError::cancelled()),
                Err(error) => {
                    sink.push(LlmStreamEvent::Reset);
                    if !self.may_retry(retry) {
                        return Err(error);
                    }
                    tracing::warn!(
                        retry = retry.saturating_add(1),
                        retry_forever = self.retry_limit.is_none(),
                        %error,
                        "retrying failed model stream"
                    );
                    self.backoff(retry, cancel).await?;
                    retry = retry.saturating_add(1);
                }
            }
        }
    }

    /// Completes one non-streaming assistant turn, for compaction.
    pub async fn complete_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn> {
        // `tool_choice: none` is what keeps the model answering with the summary
        // rather than a tool call; the tools still travel so the request shares
        // the conversation's cached prefix.
        let request = canonical_request(messages, tools, None, false, Some("none"));
        let http = self.shape_request(&request, cancel).await?;
        let mut retry = 0_u64;

        loop {
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::cancelled()),
                outcome = self.complete_once(&http, cancel) => outcome,
            };
            match outcome {
                Ok(turn) => return Ok(turn),
                Err(_) if cancel.is_cancelled() => return Err(AgentError::cancelled()),
                Err(error) => {
                    if !self.may_retry(retry) {
                        return Err(error);
                    }
                    tracing::warn!(
                        retry = retry.saturating_add(1),
                        retry_forever = self.retry_limit.is_none(),
                        %error,
                        "retrying failed model completion"
                    );
                    self.backoff(retry, cancel).await?;
                    retry = retry.saturating_add(1);
                }
            }
        }
    }
}

/// Builds the canonical, provider-neutral request the Component shapes.
///
/// The thinking level travels already spelled for the wire; `tool_choice` is
/// `Some("none")` only for compaction, and absent otherwise so a normal turn
/// sends exactly what the old client did.
fn canonical_request(
    messages: &[Message],
    tools: &Value,
    thinking: Option<ThinkingLevel>,
    stream: bool,
    tool_choice: Option<&str>,
) -> Value {
    json!({
        "thinking": thinking.map(ThinkingLevel::wire),
        "stream": stream,
        "toolChoice": tool_choice,
        "messages": messages,
        "tools": tools,
    })
}

/// Loads the bundled model provider's actor, if the plugin is enabled.
///
/// Returns `None` when the plugin is disabled or missing, or when its Component
/// fails to load; the caller then reports the provider as unavailable and a run
/// fails with a clear message instead of silently using no model.
pub async fn load_llm_provider(
    catalogue: &PluginCatalogue,
    host_capabilities: Arc<dyn crate::harness::ports::ToolRuntime>,
    home: &Path,
) -> Option<Arc<ComponentActor>> {
    let plugin = catalogue
        .global()
        .iter()
        .find(|plugin| plugin.id == LLM_PROVIDER_PLUGIN_ID)?;
    let manifest = plugin.manifest.wasm_runtime()?;
    let root = plugins::global_configuration_root(home);
    let hub = match CapabilityHub::new(
        root.clone(),
        root.clone(),
        manifest.permissions.clone(),
        host_capabilities,
    ) {
        Ok(hub) => hub,
        Err(error) => {
            tracing::warn!(%error, "model provider capabilities could not be built");
            return None;
        }
    };
    match ComponentActor::load(plugin.root.clone(), manifest, hub).await {
        Ok(actor) => Some(actor),
        Err(error) => {
            tracing::warn!(%error, "model provider failed to load");
            None
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

    #[test]
    fn retry_delay_grows_exponentially_and_stops_at_thirty_seconds() {
        assert_eq!(retry_delay(0), Duration::from_millis(250));
        assert_eq!(retry_delay(4), Duration::from_secs(4));
        assert_eq!(retry_delay(7), Duration::from_secs(30));
        assert_eq!(retry_delay(100), Duration::from_secs(30));
    }

    #[test]
    fn the_canonical_request_carries_the_wire_thinking_spelling() {
        let request = canonical_request(
            &[Message::user("hi")],
            &json!([]),
            Some(ThinkingLevel::Xhigh),
            true,
            None,
        );
        assert_eq!(request["thinking"], "xhigh");
        assert_eq!(request["stream"], true);
        assert!(request["toolChoice"].is_null());
    }

    #[test]
    fn an_unset_thinking_level_is_sent_as_null() {
        let request = canonical_request(&[], &json!([]), None, true, None);
        assert!(request["thinking"].is_null());
    }

    #[test]
    fn a_compaction_request_pins_tool_choice_to_none() {
        let request = canonical_request(&[], &json!([]), None, false, Some("none"));
        assert_eq!(request["stream"], false);
        assert_eq!(request["toolChoice"], "none");
    }

    #[test]
    fn a_stream_chunk_decodes_events_and_a_turn() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"events":[{"type":"content","text":"hi"}],"done":true,"turn":{"content":"hi","tool_calls":[],"usage":null,"finish_reason":"stop"}}"#,
        )
        .expect("the chunk decodes");
        assert!(chunk.done);
        assert!(matches!(&chunk.events[0], StreamEvent::Content { text } if text == "hi"));
        assert_eq!(chunk.turn.expect("a turn").content, "hi");
    }
}
