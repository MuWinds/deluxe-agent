//! OpenAI Responses codec.
//!
//! The Responses API is item-oriented rather than message-oriented: a turn's
//! input is a flat list of items (`message`, `function_call`,
//! `function_call_output`), the system prompt moves to a top-level
//! `instructions` string, and the stream is a sequence of *named* events whose
//! payloads carry a `type` of their own.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::canonical::{
    content_text, AssistantTurn, CanonicalRequest, Event, HttpRequest, PromptTokensDetails, Usage,
};
use super::sse::{data_payload, SseBuffer};
use super::PushResult;
use crate::config::Provider;

/// Shapes one turn into a `/responses` request.
pub fn build_request(
    provider: &Provider,
    key: &str,
    request: &CanonicalRequest,
) -> Result<HttpRequest, String> {
    let url = format!("{}/responses", provider.base_url.trim_end_matches('/'));
    let mut body = json!({
        "model": provider.model,
        "input": input_items(&request.messages),
    });
    let instructions = instructions(&request.messages);
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions);
    }
    let tools = tools(&request.tools);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if request.stream {
        body["stream"] = json!(true);
    }
    if let Some(effort) = request.thinking.as_deref().and_then(effort) {
        body["reasoning"] = json!({ "effort": effort });
    }
    if let Some(choice) = &request.tool_choice {
        body["tool_choice"] = json!(choice);
    }
    if let Some(tokens) = provider.max_output_tokens {
        body["max_output_tokens"] = json!(tokens);
    }

    let mut headers = BTreeMap::new();
    headers.insert("authorization".into(), format!("Bearer {key}"));
    headers.insert("content-type".into(), "application/json".into());
    Ok(HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body: serde_json::to_string(&body).map_err(|error| error.to_string())?,
    })
}

/// The reasoning effort Responses understands, or `None` to send nothing.
///
/// Responses has no `none`/`xhigh`/`max`; the top of the scale collapses to
/// `high`, and the explicit "off" choice is expressed by omitting the field.
fn effort(level: &str) -> Option<&'static str> {
    match level {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// Joins every system message into the top-level `instructions` string.
fn instructions(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .map(|message| content_text(message.get("content").unwrap_or(&Value::Null)))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Converts canonical messages into Responses input items.
fn input_items(messages: &[Value]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => items.push(json!({
                "type": "message",
                "role": "user",
                "content": user_content(message.get("content")),
            })),
            Some("assistant") => {
                let text = content_text(message.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
                        "arguments": call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}"),
                    }));
                }
            }
            Some("tool") => items.push(json!({
                "type": "function_call_output",
                "call_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                "output": content_text(message.get("content").unwrap_or(&Value::Null)),
            })),
            _ => {}
        }
    }
    items
}

/// A user message's content blocks.
fn user_content(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({ "type": "input_text", "text": text })],
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("image_url") {
                    let url = part
                        .pointer("/image_url/url")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    json!({ "type": "input_image", "image_url": url })
                } else {
                    json!({
                        "type": "input_text",
                        "text": part.get("text").and_then(Value::as_str).unwrap_or(""),
                    })
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Converts chat-style tool schemas into Responses function tools.
fn tools(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|tool| {
                    let function = tool.get("function")?;
                    let name = function.get("name")?.as_str()?;
                    let mut entry = json!({
                        "type": "function",
                        "name": name,
                        "parameters": function
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object" })),
                    });
                    if let Some(description) = function.get("description") {
                        if !description.is_null() {
                            entry["description"] = description.clone();
                        }
                    }
                    Some(entry)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fragments of one function call, keyed by its output index.
#[derive(Default)]
struct CallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// One stream's accumulated Responses turn.
#[derive(Default)]
pub struct State {
    buffer: SseBuffer,
    content: String,
    calls: BTreeMap<usize, CallAcc>,
    usage: Option<Usage>,
    /// Set only when the model ran out of room, so it overrides the
    /// tool-call/stop default.
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
                "response.output_text.delta" => {
                    if let Some(text) = event.get("delta").and_then(Value::as_str) {
                        if !text.is_empty() {
                            self.content.push_str(text);
                            events.push(Event::Content {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    if let Some(text) = event.get("delta").and_then(Value::as_str) {
                        if !text.is_empty() {
                            events.push(Event::Reasoning {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "response.output_item.added" => {
                    if let Some(item) = event.get("item") {
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            let index = event
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as usize;
                            self.calls.insert(
                                index,
                                CallAcc {
                                    id: item
                                        .get("call_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    name: item
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    arguments: item
                                        .get("arguments")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                },
                            );
                        }
                    }
                }
                "response.function_call_arguments.delta" => {
                    let index = event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        self.calls
                            .entry(index)
                            .or_default()
                            .arguments
                            .push_str(delta);
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = event.get("item") {
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            let index = event
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as usize;
                            let acc = self.calls.entry(index).or_default();
                            if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                                acc.id = id.to_string();
                            }
                            if let Some(name) = item.get("name").and_then(Value::as_str) {
                                acc.name = name.to_string();
                            }
                            if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                                acc.arguments = arguments.to_string();
                            }
                        }
                    }
                }
                "response.completed" => {
                    if let Some(usage) = event.pointer("/response/usage") {
                        self.usage = Some(responses_usage(usage));
                    }
                    self.done = true;
                }
                "response.incomplete" => {
                    if let Some(usage) = event.pointer("/response/usage") {
                        self.usage = Some(responses_usage(usage));
                    }
                    self.finish_reason = Some("length".into());
                    self.done = true;
                }
                "response.failed" => {
                    let detail = event
                        .pointer("/response/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("the response failed");
                    return Err(format!("Provider error: {detail}"));
                }
                "error" => {
                    let detail = event
                        .get("message")
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
        let calls: Vec<_> = self
            .calls
            .into_values()
            .map(|acc| (acc.id, acc.name, acc.arguments))
            .collect();
        let finish_reason = self.finish_reason.or_else(|| {
            Some(if calls.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            })
        });
        AssistantTurn::from_calls(self.content, calls, self.usage, finish_reason)
    }
}

/// Maps a Responses usage object onto the canonical one.
fn responses_usage(value: &Value) -> Usage {
    Usage {
        prompt_tokens: value.get("input_tokens").and_then(Value::as_u64),
        completion_tokens: value.get("output_tokens").and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
        prompt_tokens_details: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .map(|cached| PromptTokensDetails {
                cached_tokens: Some(cached),
            }),
        ..Default::default()
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
    for item in payload
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        content.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                }
            }
            Some("function_call") => calls.push((
                item.get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                item.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                item.get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string(),
            )),
            _ => {}
        }
    }
    let usage = payload.get("usage").map(responses_usage);
    let finish_reason = if !calls.is_empty() {
        Some("tool_calls".to_string())
    } else if payload
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        == Some("max_output_tokens")
    {
        Some("length".to_string())
    } else {
        Some("stop".to_string())
    };
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
            protocol: "openai-responses".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            api_key: String::new(),
            supports_images: true,
            context_limit: 128_000,
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
    fn the_system_message_becomes_instructions_and_tools_are_reshaped() {
        let http = build_request(&provider(), "k", &request()).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");

        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(http.url, "https://api.openai.com/v1/responses");
        assert_eq!(http.headers["authorization"], "Bearer k");
    }

    #[test]
    fn the_top_of_the_effort_scale_collapses_to_high() {
        let mut request = request();
        request.thinking = Some("max".into());
        let http = build_request(&provider(), "k", &request).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn an_off_thinking_level_sends_no_reasoning_object() {
        let mut request = request();
        request.thinking = Some("none".into());
        let http = build_request(&provider(), "k", &request).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn text_and_reasoning_deltas_are_surfaced() {
        let mut state = State::default();
        let events = feed(
            &mut state,
            &[
                r#"{"type":"response.reasoning_summary_text.delta","delta":"why"}"#,
                r#"{"type":"response.output_text.delta","delta":"answer"}"#,
            ],
        );
        assert!(matches!(&events[0], Event::Reasoning { text } if text == "why"));
        assert!(matches!(&events[1], Event::Content { text } if text == "answer"));
    }

    #[test]
    fn function_call_arguments_accumulate_and_finish_as_tool_calls() {
        let mut state = State::default();
        let payloads = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"read_file","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"pa"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"th\":\"a.txt\"}"}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}"#,
        ];
        feed(&mut state, &payloads);
        assert!(state.done);

        let turn = state.turn();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].function.name, "read_file");
        assert_eq!(turn.tool_calls[0].function.arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(turn.finish_reason.as_deref(), Some("tool_calls"));
        let usage = turn.usage.expect("usage was reported");
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            Some(4)
        );
    }

    #[test]
    fn an_incomplete_response_reports_a_length_finish() {
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"response.incomplete","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        assert!(state.done);
        assert_eq!(state.turn().finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn a_failed_response_is_an_error() {
        let mut state = State::default();
        let error = state
            .push(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n")
            .expect_err("a failed response is refused");
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn a_non_streaming_body_decodes_into_a_turn() {
        let turn = parse_complete(
            br#"{"output":[{"type":"message","content":[{"type":"output_text","text":"summary"}]}],"usage":{"input_tokens":3,"output_tokens":1},"status":"completed"}"#,
        )
        .expect("the body decodes");
        assert_eq!(turn.content, "summary");
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
        assert_eq!(turn.usage.expect("usage").prompt_tokens, Some(3));
    }
}
