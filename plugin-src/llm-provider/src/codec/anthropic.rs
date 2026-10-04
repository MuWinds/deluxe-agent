//! Anthropic Messages codec.
//!
//! Anthropic differs from the OpenAI shapes in ways that are easy to get wrong:
//!
//! * `max_tokens` is required, and `thinking.budget_tokens` must be smaller than
//!   it, so a budget that would meet or exceed the ceiling raises the ceiling.
//! * a tool's input is a JSON *object*, not a string, so the canonical
//!   `arguments` string is parsed before it goes on the wire;
//! * images are base64 blocks with a media type, so a canonical data URL is
//!   split into its parts;
//! * a tool result is a `tool_result` block inside a `user` message, and
//!   consecutive results must be merged into one user turn;
//! * the stream is a sequence of named events (`message_start`,
//!   `content_block_delta`, …).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::canonical::{
    content_text, split_data_url, AssistantTurn, CanonicalRequest, Event, HttpRequest,
    PromptTokensDetails, Usage,
};
use super::sse::{data_payload, SseBuffer};
use super::PushResult;
use crate::config::Provider;

/// The default output ceiling when a profile sets none. Anthropic requires the
/// field, so it cannot be omitted.
const DEFAULT_MAX_TOKENS: u32 = 4096;
/// The API version header every request must carry.
const API_VERSION: &str = "2023-06-01";

/// Shapes one turn into a `/v1/messages` request.
pub fn build_request(
    provider: &Provider,
    key: &str,
    request: &CanonicalRequest,
) -> Result<HttpRequest, String> {
    let base = provider.base_url.trim_end_matches('/');
    // Tolerate a base URL that already ends in `/v1`, so the profile does not
    // have to know which half of the path the version segment belongs to.
    let url = if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };

    let mut body = json!({
        "model": provider.model,
        "messages": convert_messages(&request.messages)?,
    });
    let system = instructions(&request.messages);
    if !system.is_empty() {
        body["system"] = json!(system);
    }

    // `tool_choice: none` has no Anthropic spelling; dropping the tools is what
    // actually stops the model from calling one.
    let mut tools = convert_tools(&request.tools);
    let mut tool_choice = request.tool_choice.clone();
    if tool_choice.as_deref() == Some("none") {
        tools.clear();
        tool_choice = None;
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(choice) = tool_choice {
        body["tool_choice"] = json!({ "type": choice });
    }

    let mut max_tokens = provider.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    if let Some(budget) = request.thinking.as_deref().and_then(budget) {
        // The API rejects `budget_tokens >= max_tokens`, so a budget that would
        // meet the ceiling raises the ceiling instead of failing the request.
        if max_tokens <= budget {
            max_tokens = budget + 1024;
        }
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    body["max_tokens"] = json!(max_tokens);
    if request.stream {
        body["stream"] = json!(true);
    }

    let mut headers = BTreeMap::new();
    headers.insert("x-api-key".into(), key.to_string());
    headers.insert("anthropic-version".into(), API_VERSION.into());
    headers.insert("content-type".into(), "application/json".into());
    Ok(HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body: serde_json::to_string(&body).map_err(|error| error.to_string())?,
    })
}

/// The thinking budget a level maps to, or `None` for thinking off.
///
/// `none` and an unset level both mean "no thinking block"; the strengths in
/// between are a fixed ladder, because Anthropic takes a token count rather than
/// a named level.
fn budget(level: &str) -> Option<u32> {
    match level {
        "minimal" => Some(1024),
        "low" => Some(2048),
        "medium" => Some(4096),
        "high" => Some(8192),
        "xhigh" => Some(16384),
        "max" => Some(32768),
        _ => None,
    }
}

/// Joins every system message into the top-level `system` string.
fn instructions(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .map(|message| content_text(message.get("content").unwrap_or(&Value::Null)))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Converts canonical messages into Anthropic messages.
///
/// Consecutive tool results are merged into one `user` turn, because Anthropic
/// requires the results of parallel calls to share a single message.
fn convert_messages(messages: &[Value]) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("system") => {}
            Some("user") => out.push(json!({
                "role": "user",
                "content": user_blocks(message.get("content")),
            })),
            Some("assistant") => {
                let blocks = assistant_blocks(message);
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Some("tool") => {
                let block = tool_result_block(message);
                if let Some(last) = out.last_mut() {
                    if last.get("role").and_then(Value::as_str) == Some("user")
                        && only_tool_results(last)
                    {
                        if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
                        {
                            content.push(block);
                            continue;
                        }
                    }
                }
                out.push(json!({ "role": "user", "content": [block] }));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Whether a message's content is entirely `tool_result` blocks.
fn only_tool_results(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// A user message's content blocks.
fn user_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
        Some(Value::Array(parts)) => parts.iter().map(part_block).collect(),
        _ => Vec::new(),
    }
}

/// One canonical content part as an Anthropic block.
fn part_block(part: &Value) -> Value {
    if part.get("type").and_then(Value::as_str) == Some("image_url") {
        let url = part
            .pointer("/image_url/url")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some((media_type, data)) = split_data_url(url) {
            return json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            });
        }
        return json!({
            "type": "text",
            "text": format!("[an image the host could not inline: {url}]"),
        });
    }
    json!({
        "type": "text",
        "text": part.get("text").and_then(Value::as_str).unwrap_or(""),
    })
}

/// An assistant message's content blocks, including its tool calls.
fn assistant_blocks(message: &Value) -> Vec<Value> {
    let mut blocks = Vec::new();
    let text = content_text(message.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        // The API requires an object. A call whose arguments did not parse
        // cannot have executed, so an empty object loses nothing that mattered.
        let input = serde_json::from_str::<Value>(arguments)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
            "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
            "input": input,
        }));
    }
    blocks
}

/// A `tool` message as a `tool_result` block.
fn tool_result_block(message: &Value) -> Value {
    let content = match message.get("content") {
        Some(Value::Array(parts)) => Value::Array(parts.iter().map(part_block).collect()),
        other => json!(content_text(other.unwrap_or(&Value::Null))),
    };
    json!({
        "type": "tool_result",
        "tool_use_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
        "content": content,
    })
}

/// Converts chat-style tool schemas into Anthropic tools.
fn convert_tools(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|tool| {
                    let function = tool.get("function")?;
                    let name = function.get("name")?.as_str()?;
                    Some(json!({
                        "name": name,
                        "description": function
                            .get("description")
                            .cloned()
                            .unwrap_or_else(|| json!("")),
                        "input_schema": function
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object" })),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One content block's accumulated state.
#[derive(Default)]
struct BlockAcc {
    name: String,
    id: String,
    arguments: String,
}

/// One stream's accumulated Anthropic turn.
#[derive(Default)]
pub struct State {
    buffer: SseBuffer,
    content: String,
    calls: BTreeMap<usize, BlockAcc>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read: Option<u64>,
    finish_reason: Option<String>,
    done: bool,
}

impl State {
    /// Feeds one response chunk and returns the decoded fragments.
    pub fn push(&mut self, chunk: &[u8]) -> Result<PushResult, String> {
        let mut events = Vec::new();
        for line in self.buffer.push(chunk) {
            let Some(payload) = data_payload(&line) else {
                continue;
            };
            let event: Value = serde_json::from_str(payload)
                .map_err(|error| format!("Malformed SSE payload: {error} — {payload}"))?;
            match event.get("type").and_then(Value::as_str).unwrap_or("") {
                "message_start" => {
                    if let Some(usage) = event.pointer("/message/usage") {
                        self.input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
                        self.cache_read =
                            usage.get("cache_read_input_tokens").and_then(Value::as_u64);
                    }
                }
                "content_block_start" => {
                    if let Some(block) = event.get("content_block") {
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            let index =
                                event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                            self.calls.insert(
                                index,
                                BlockAcc {
                                    id: block
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    name: block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    arguments: String::new(),
                                },
                            );
                        }
                    }
                }
                "content_block_delta" => {
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let Some(delta) = event.get("delta") else {
                        continue;
                    };
                    match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    self.content.push_str(text);
                                    events.push(Event::Content {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    events.push(Event::Reasoning {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                            {
                                self.calls
                                    .entry(index)
                                    .or_default()
                                    .arguments
                                    .push_str(partial);
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(reason) =
                        event.pointer("/delta/stop_reason").and_then(Value::as_str)
                    {
                        self.finish_reason = Some(stop_reason(reason).to_string());
                    }
                    if let Some(output) = event
                        .pointer("/usage/output_tokens")
                        .and_then(Value::as_u64)
                    {
                        self.output_tokens = Some(output);
                    }
                }
                "message_stop" => self.done = true,
                "error" => {
                    let detail = event
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(format!("Provider error: {detail}"));
                }
                _ => {}
            }
        }
        Ok(PushResult {
            events,
            done: self.done,
        })
    }

    /// Consumes the accumulated state into the final turn.
    pub fn turn(self) -> AssistantTurn {
        // Read the usage before the tool-call map is moved out of `self`.
        let usage = self.usage();
        let calls: Vec<_> = self
            .calls
            .into_values()
            .filter(|acc| !acc.name.is_empty())
            .map(|acc| (acc.id, acc.name, acc.arguments))
            .collect();
        let finish_reason = self.finish_reason.or_else(|| {
            Some(if calls.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            })
        });
        AssistantTurn::from_calls(self.content, calls, usage, finish_reason)
    }

    /// The usage figures the stream reported, if any.
    fn usage(&self) -> Option<Usage> {
        if self.input_tokens.is_none() && self.output_tokens.is_none() {
            return None;
        }
        Some(Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: match (self.input_tokens, self.output_tokens) {
                (Some(input), Some(output)) => Some(input + output),
                _ => None,
            },
            prompt_tokens_details: self.cache_read.map(|cached| PromptTokensDetails {
                cached_tokens: Some(cached),
            }),
            ..Default::default()
        })
    }
}

/// Normalises an Anthropic stop reason to the canonical vocabulary.
fn stop_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

/// Decodes a non-streaming response body into a turn.
pub fn parse_complete(body: &[u8]) -> Result<AssistantTurn, String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|error| format!("Malformed response: {error}"))?;
    if let Some(error) = payload.get("error") {
        return Err(format!("Provider error: {error}"));
    }
    let mut content = String::new();
    let mut calls = Vec::new();
    for block in payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                content.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""))
            }
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                calls.push((
                    block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string()),
                ));
            }
            _ => {}
        }
    }
    let usage = payload.get("usage").map(|usage| Usage {
        prompt_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        completion_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        total_tokens: None,
        prompt_tokens_details: usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .map(|cached| PromptTokensDetails {
                cached_tokens: Some(cached),
            }),
        ..Default::default()
    });
    let finish_reason = payload
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(|reason| stop_reason(reason).to_string());
    Ok(AssistantTurn::from_calls(
        content,
        calls,
        usage,
        finish_reason,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::canonical::CanonicalRequest;

    fn provider() -> Provider {
        Provider {
            id: "p".into(),
            name: "P".into(),
            protocol: "anthropic-messages".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-5".into(),
            api_key: String::new(),
            supports_images: true,
            context_limit: 200_000,
            max_output_tokens: None,
        }
    }

    fn request() -> CanonicalRequest {
        CanonicalRequest {
            thinking: None,
            stream: true,
            tool_choice: None,
            messages: vec![
                json!({"role": "system", "content": "be brief"}),
                json!({"role": "user", "content": "hi"}),
            ],
            tools: json!([{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object"}
                }
            }]),
        }
    }

    fn feed(state: &mut State, payloads: &[&str]) -> Vec<Event> {
        let mut events = Vec::new();
        for payload in payloads {
            let chunk = format!("data: {payload}\n\n");
            events.extend(
                state
                    .push(chunk.as_bytes())
                    .expect("the chunk decodes")
                    .events,
            );
        }
        events
    }

    #[test]
    fn the_version_segment_is_not_doubled() {
        let mut provider = provider();
        provider.base_url = "https://api.anthropic.com/v1".into();
        let http = build_request(&provider, "k", &request()).expect("the request builds");
        assert_eq!(http.url, "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn the_system_message_moves_to_the_top_level_and_tools_are_reshaped() {
        let http = build_request(&provider(), "k", &request()).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");

        assert_eq!(body["system"], "be brief");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(http.headers["x-api-key"], "k");
        assert_eq!(http.headers["anthropic-version"], API_VERSION);
    }

    #[test]
    fn max_tokens_is_always_present_and_defaults_to_the_required_floor() {
        let http = build_request(&provider(), "k", &request()).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn a_thinking_budget_raises_the_ceiling_it_would_otherwise_meet() {
        // `budget_tokens` must be strictly below `max_tokens`, so a high budget
        // with the default ceiling has to lift the ceiling.
        let mut request = request();
        request.thinking = Some("max".into());
        let http = build_request(&provider(), "k", &request).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 32768);
        assert!(body["max_tokens"].as_u64().unwrap() > 32768);
    }

    #[test]
    fn off_thinking_sends_no_thinking_block() {
        let mut request = request();
        request.thinking = Some("none".into());
        let http = build_request(&provider(), "k", &request).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn a_none_tool_choice_drops_the_tools_because_anthropic_has_no_none() {
        let mut request = request();
        request.tool_choice = Some("none".into());
        let http = build_request(&provider(), "k", &request).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn an_image_data_url_becomes_a_base64_source_block() {
        let message = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]
        });
        let blocks = user_blocks(message.get("content"));
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn an_assistant_tool_call_becomes_a_tool_use_block_with_an_object_input() {
        let message = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}
            }]
        });
        let blocks = assistant_blocks(&message);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_1");
        assert_eq!(blocks[0]["input"]["path"], "a.txt");
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let messages = vec![
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "x", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "y", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "one"}),
            json!({"role": "tool", "tool_call_id": "b", "content": "two"}),
        ];
        let converted = convert_messages(&messages).expect("the messages convert");

        assert_eq!(converted.len(), 2, "the two results share one user turn");
        let results = converted[1]["content"].as_array().expect("an array");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "a");
        assert_eq!(results[1]["tool_use_id"], "b");
    }

    #[test]
    fn text_and_thinking_and_input_deltas_are_surfaced() {
        let mut state = State::default();
        let events = feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":4}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert!(matches!(&events[0], Event::Reasoning { text } if text == "why"));
        assert!(matches!(&events[1], Event::Content { text } if text == "answer"));
        assert!(state.done);

        let turn = state.turn();
        assert_eq!(turn.content, "answer");
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
        let usage = turn.usage.expect("usage was reported");
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(2));
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            Some(4)
        );
    }

    #[test]
    fn a_tool_use_block_accumulates_partial_json_and_normalises_the_stop() {
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"read_file"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\"a.txt\"}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let turn = state.turn();

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].function.name, "read_file");
        assert_eq!(turn.tool_calls[0].function.arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(turn.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn an_error_event_is_an_error() {
        let mut state = State::default();
        let error = state
            .push(b"data: {\"type\":\"error\",\"error\":{\"message\":\"boom\"}}\n")
            .expect_err("an error event is refused");
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn a_non_streaming_body_decodes_into_a_turn() {
        let turn = parse_complete(
            br#"{"content":[{"type":"text","text":"summary"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":2}}"#,
        )
        .expect("the body decodes");
        assert_eq!(turn.content, "summary");
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            turn.usage
                .expect("usage")
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens),
            Some(2)
        );
    }
}
