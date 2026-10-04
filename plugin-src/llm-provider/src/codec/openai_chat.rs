//! OpenAI chat-completions codec.
//!
//! This is the shape the host has always spoken, so its canonical types pass
//! through almost untouched: only the vendor's base URL, model, key, and output
//! budget are added here.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::canonical::{AssistantTurn, CanonicalRequest, Event, HttpRequest, Usage};
use super::sse::{data_payload, SseBuffer};
use super::PushResult;
use crate::config::Provider;

/// Shapes one turn into a `/chat/completions` request.
pub fn build_request(
    provider: &Provider,
    key: &str,
    request: &CanonicalRequest,
) -> Result<HttpRequest, String> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let mut body = json!({
        "model": provider.model,
        "messages": request.messages,
        "tools": request.tools,
    });
    if request.stream {
        body["stream"] = json!(true);
        // Asks the provider to emit a final chunk carrying token counts.
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(thinking) = &request.thinking {
        body["reasoning_effort"] = json!(thinking);
    }
    if let Some(choice) = &request.tool_choice {
        body["tool_choice"] = json!(choice);
    }
    if let Some(tokens) = provider.max_output_tokens {
        body["max_tokens"] = json!(tokens);
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

/// Fragments of one tool call, reassembled across chunks.
#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// One stream's accumulated chat-completions turn.
#[derive(Default)]
pub struct State {
    buffer: SseBuffer,
    content: String,
    calls: BTreeMap<usize, ToolCallAcc>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    done: bool,
    saw_choice: bool,
}

impl State {
    /// Feeds one response chunk and returns the decoded fragments.
    pub fn push(&mut self, chunk: &[u8]) -> Result<PushResult, String> {
        let mut events = Vec::new();
        for line in self.buffer.push(chunk) {
            let Some(payload) = data_payload(&line) else {
                continue;
            };
            if payload == "[DONE]" {
                self.done = true;
                break;
            }
            let event: Value = serde_json::from_str(payload)
                .map_err(|error| format!("Malformed SSE payload: {error} — {payload}"))?;
            if let Some(error) = event.get("error") {
                return Err(format!("Provider error: {error}"));
            }
            if let Some(parsed) = event
                .get("usage")
                .and_then(|value| serde_json::from_value::<Usage>(value.clone()).ok())
            {
                self.usage = Some(parsed);
            }
            let Some(choice) = event.get("choices").and_then(|choices| choices.get(0)) else {
                continue;
            };
            self.saw_choice = true;
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(fragment) = delta.get("content").and_then(Value::as_str) {
                if !fragment.is_empty() {
                    self.content.push_str(fragment);
                    events.push(Event::Content {
                        text: fragment.to_string(),
                    });
                }
            }
            // DeepSeek-R1-style models stream the chain of thought in a field
            // of its own; other providers spell it `reasoning`. It is forwarded
            // rather than accumulated, because the loop never sends reasoning
            // back to the model.
            let reasoning = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str);
            if let Some(fragment) = reasoning {
                if !fragment.is_empty() {
                    events.push(Event::Reasoning {
                        text: fragment.to_string(),
                    });
                }
            }
            absorb_tool_calls(delta, &mut self.calls);
        }

        // `[DONE]` with no choice means the endpoint answered with something
        // that is not a completion; reporting it now is the last chance the
        // decoder has to tell a real turn from an empty stream.
        if self.done && !self.saw_choice {
            return Err("Stream completed without a choice".into());
        }
        Ok(PushResult {
            events,
            done: self.done,
        })
    }

    /// Consumes the accumulated state into the final turn.
    pub fn turn(self) -> AssistantTurn {
        let calls = self
            .calls
            .into_values()
            .map(|acc| (acc.id, acc.name, acc.arguments))
            .collect();
        AssistantTurn::from_calls(self.content, calls, self.usage, self.finish_reason)
    }
}

/// Folds one `delta` object into the per-index tool-call accumulators.
///
/// `function.arguments` is a string that must be concatenated across chunks —
/// it is not valid JSON until the turn ends.
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

/// Decodes a non-streaming response body into a turn.
pub fn parse_complete(body: &[u8]) -> Result<AssistantTurn, String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|error| format!("Malformed response: {error}"))?;
    if let Some(error) = payload.get("error") {
        return Err(format!("Provider error: {error}"));
    }
    let choice = payload
        .get("choices")
        .and_then(|choices| choices.get(0))
        .ok_or_else(|| "Response carried no choices".to_string())?;
    let content = choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let usage = payload
        .get("usage")
        .and_then(|value| serde_json::from_value::<Usage>(value.clone()).ok());
    Ok(AssistantTurn {
        content,
        tool_calls: Vec::new(),
        usage,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::canonical::CanonicalRequest;

    fn provider() -> Provider {
        Provider {
            id: "p".into(),
            name: "P".into(),
            protocol: "openai-chat".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "m".into(),
            api_key: String::new(),
            supports_images: false,
            context_limit: 0,
            max_output_tokens: None,
        }
    }

    fn request() -> CanonicalRequest {
        CanonicalRequest {
            thinking: None,
            stream: true,
            tool_choice: None,
            messages: vec![json!({"role": "user", "content": "hi"})],
            tools: json!([]),
        }
    }

    /// Feeds raw SSE `data:` payloads, exactly as the host would.
    fn feed(payloads: &[&str]) -> (Vec<Event>, bool) {
        let mut state = State::default();
        let mut events = Vec::new();
        let mut done = false;
        for payload in payloads {
            let chunk = format!("data: {payload}\n\n");
            let result = state.push(chunk.as_bytes()).expect("the chunk decodes");
            events.extend(result.events);
            done = result.done;
        }
        (events, done)
    }

    #[test]
    fn reassembles_arguments_split_across_chunks() {
        // `arguments` is not valid JSON until the last fragment lands, so a
        // naive per-chunk parse would truncate the call.
        let mut state = State::default();
        for payload in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]}}]}"#,
            "[DONE]",
        ] {
            state
                .push(format!("data: {payload}\n\n").as_bytes())
                .expect("the chunk decodes");
        }
        let turn = state.turn();

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].function.name, "read_file");
        assert_eq!(turn.tool_calls[0].function.arguments, r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn keeps_parallel_tool_calls_ordered_by_index() {
        let mut state = State::default();
        for payload in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
            "[DONE]",
        ] {
            state
                .push(format!("data: {payload}\n\n").as_bytes())
                .expect("the chunk decodes");
        }
        let turn = state.turn();

        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].function.name, "read_file");
        assert_eq!(turn.tool_calls[1].function.name, "list_dir");
    }

    #[test]
    fn tolerates_a_provider_that_repeats_the_function_name() {
        let mut state = State::default();
        for payload in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"exec","arguments":"{\"comm"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"exec","arguments":"and\":\"ls\"}"}}]}}]}"#,
            "[DONE]",
        ] {
            state
                .push(format!("data: {payload}\n\n").as_bytes())
                .expect("the chunk decodes");
        }
        let turn = state.turn();

        assert_eq!(turn.tool_calls[0].function.name, "exec");
        assert_eq!(turn.tool_calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn reasoning_and_content_stay_in_separate_events() {
        let (events, _) = feed(&[
            r#"{"choices":[{"delta":{"reasoning_content":"why"}}]}"#,
            r#"{"choices":[{"delta":{"content":"answer"}}]}"#,
        ]);
        assert!(matches!(&events[0], Event::Reasoning { text } if text == "why"));
        assert!(matches!(&events[1], Event::Content { text } if text == "answer"));
    }

    #[test]
    fn done_only_follows_the_sentinel_and_carries_the_turn() {
        let mut state = State::default();
        let before = state
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n")
            .expect("decodes");
        assert!(!before.done);
        let after = state.push(b"data: [DONE]\n").expect("decodes");
        assert!(after.done);
    }

    #[test]
    fn deepseek_reports_cache_hits_at_the_top_level() {
        let mut state = State::default();
        state
            .push(
                b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"prompt_cache_hit_tokens\":80}}\n",
            )
            .expect("decodes");
        state.push(b"data: [DONE]\n").expect("decodes");
        let turn = state.turn();

        let usage = turn.usage.expect("usage was reported");
        assert_eq!(usage.prompt_tokens, Some(100));
        assert_eq!(usage.prompt_cache_hit_tokens, Some(80));
    }

    #[test]
    fn a_stream_that_ends_without_a_choice_is_an_error() {
        let mut state = State::default();
        let error = state
            .push(b"data: [DONE]\n")
            .expect_err("a choice-less stream is refused");
        assert!(error.contains("without a choice"), "{error}");
    }

    #[test]
    fn a_chosen_thinking_level_and_output_budget_reach_the_body() {
        let mut request = request();
        request.thinking = Some("xhigh".into());
        let mut provider = provider();
        provider.max_output_tokens = Some(8192);
        let http = build_request(&provider, "k", &request).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");

        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(http.headers["authorization"], "Bearer k");
        assert_eq!(http.url, "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn an_unset_thinking_level_leaves_the_parameter_out() {
        let http = build_request(&provider(), "k", &request()).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn a_non_streaming_completion_omits_the_stream_options() {
        let mut request = request();
        request.stream = false;
        request.tool_choice = Some("none".into());
        let http = build_request(&provider(), "k", &request).expect("the request builds");
        let body: Value = serde_json::from_str(&http.body).expect("body is JSON");
        assert!(body.get("stream").is_none());
        assert!(body.get("stream_options").is_none());
        assert_eq!(body["tool_choice"], "none");
    }

    #[test]
    fn a_non_streaming_body_decodes_into_a_turn() {
        let turn = parse_complete(
            br#"{"choices":[{"message":{"content":"summary"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3}}"#,
        )
        .expect("the body decodes");
        assert_eq!(turn.content, "summary");
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
        assert_eq!(turn.usage.expect("usage").prompt_tokens, Some(3));
    }
}
