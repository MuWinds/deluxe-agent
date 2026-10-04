//! Canonical, provider-neutral wire types.
//!
//! The host speaks one vocabulary: the OpenAI chat-completions shape it has
//! always used. Each protocol module transforms this vocabulary into its own
//! request body and back. Keeping the canonical types here means the host never
//! learns SSE, `tool_use`, `function_call`, or any vendor's stop-reason words.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One HTTP request the host will send on the Component's behalf.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// One streamed fragment the host forwards to the UI.
///
/// Only the chain of thought and the answer are surfaced: tool calls, usage,
/// and the finish reason are accumulated inside the Component and returned as
/// the final turn, because the agent loop does not render them incrementally.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Reasoning { text: String },
    Content { text: String },
}

/// The canonical request the host sends to `build-request`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalRequest {
    /// The reasoning effort, already mapped to its wire spelling by the host
    /// (`none`, `minimal`, …), or `None` to send no parameter at all.
    #[serde(default)]
    pub thinking: Option<String>,
    pub stream: bool,
    /// `auto` for a normal turn, `none` for compaction.
    #[serde(default)]
    pub tool_choice: Option<String>,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub tools: Value,
}

/// The token counts a provider reports for one request.
///
/// Field names mirror the host's `Usage` exactly, so the final turn decodes
/// without a translation step.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// One call the model asked for, in canonical form.
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
    /// Raw JSON text, exactly as the model emitted it.
    pub arguments: String,
}

/// The result of one assistant turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantTurn {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

impl AssistantTurn {
    /// Builds the canonical turn from accumulated tool-call fragments.
    ///
    /// A call the provider left without an id gets a positional one, because the
    /// loop addresses the result back to it by id and the id only has to be
    /// unique within the turn.
    pub fn from_calls(
        content: String,
        calls: Vec<(String, String, String)>,
        usage: Option<Usage>,
        finish_reason: Option<String>,
    ) -> Self {
        let tool_calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, (id, name, arguments))| ToolCall {
                id: if id.is_empty() {
                    format!("call_{index}")
                } else {
                    id
                },
                call_type: "function".into(),
                function: FunctionCall { name, arguments },
            })
            .collect();
        Self {
            content,
            tool_calls,
            usage,
            finish_reason,
        }
    }
}

/// Splits a `data:` URL into `(media_type, base64)`, or `None` when it is not a
/// data URL.
///
/// Anthropic takes images as base64 with a media type rather than as a URL, so
/// the transform has to unpack the canonical image part.
pub fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.ends_with(";base64") {
        return None;
    }
    let media_type = meta.trim_end_matches(";base64").to_string();
    Some((media_type, data.to_string()))
}

/// The plain text a canonical content value contributes, ignoring image parts.
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                (part.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| part.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}
