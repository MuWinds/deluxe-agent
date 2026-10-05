//! Canonical, provider-neutral model types.
//!
//! This module is the vocabulary the agent loop, the IPC layer, and the model
//! provider Component all share. It is deliberately wire-shaped rather than
//! abstract: a [`Message`] serialises to exactly the JSON a chat-completions
//! request carries, and that same JSON is the canonical request the host hands
//! to the provider Component to reshape for whichever protocol is active.
//!
//! There is no HTTP client here any more. The socket, the SSE parsing, and the
//! tool-call reassembly all live in the `llm-provider` Component; what remains
//! is the shape everything agrees on.

use serde::{Deserialize, Serialize};

use crate::attachments::{Attachment, ImageRef};

/// How much reasoning effort to ask a reasoning model for.
///
/// The strengths are hardcoded because pi-ai fixes the union upstream:
/// `off | minimal | low | medium | high | xhigh | max`. Two of them are not
/// strengths: `None` (the option) omits `reasoning_effort` entirely and lets
/// the endpoint decide, while [`ThinkingLevel::Off`] sends `"none"` to ask the
/// endpoint to turn reasoning off. They are distinct choices, so both exist.
/// A provider that spells a level differently maps it; it does not add a level.
///
/// Lives beside the wire format rather than in the config because it is chosen
/// per conversation, like pi's shift+tab indicator: every session carries its
/// own level, and a run sends whichever one the session holds. Contrast the
/// input modalities, which describe the *endpoint* and so are owned by the
/// provider Component now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// Reasoning off. Sent as `"none"`, the value OpenAI-compatible endpoints
    /// take to skip chain-of-thought. Explicit rename because the variant is
    /// `Off` but the wire spelling is `none`.
    #[serde(rename = "none")]
    Off,
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
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    /// The value sent as `reasoning_effort` on the wire.
    ///
    /// The provider Component maps this spelling to its own protocol; the host
    /// never learns how any vendor spells a strength.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Off => "none",
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
            Self::Off => "关闭",
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

/// A user turn's body: text plus the paths of any attached files.
///
/// Carried as a struct rather than a bare string so the GUI can hand the
/// composer's text and its attachments over in one value, and so history replay
/// round-trips the attachments — a file pinned to one prompt travels with that
/// prompt on every later turn.
#[derive(Debug, Clone, Default)]
pub struct UserTurn {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// A text-only turn, which is what most callers mean.
///
/// Every existing prompt is a bare string, so these keep `"hi".into()` working
/// at the call sites that predate attachments.
impl From<&str> for UserTurn {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            attachments: Vec::new(),
        }
    }
}

impl From<String> for UserTurn {
    fn from(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
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

impl Message {
    /// A user turn, with any attached files listed as paths for the model.
    ///
    /// Always a plain text body: an attachment is a path, not content, so the
    /// message keeps the bare-string shape it has always had and the model
    /// reads the file itself with `read_file` (text) or `read_image` (images).
    /// The listing is deterministic, so a live turn and its replay produce
    /// byte-identical text and the provider's prefix cache stays valid.
    pub fn user_turn(turn: UserTurn) -> Self {
        let mut content = turn.text;
        if !turn.attachments.is_empty() {
            if !content.trim().is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(
                "The user attached the following file(s). Read text files with `read_file` \
                 and images with `read_image`:\n",
            );
            for attachment in &turn.attachments {
                content.push_str("- ");
                content.push_str(&attachment.path);
                content.push('\n');
            }
        }
        Self::user(content)
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
    /// For the text-only case; [`Message::user_turn`] wraps an attachment into a
    /// multimodal parts array.
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
    /// the shape the chat-completions adapter produces for a tool-result image
    /// block. A reference that cannot be read back degrades to a text note
    /// rather than failing the run: the model loses the picture, not the whole
    /// turn.
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

/// The result of one assistant turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantTurn {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_thinking_level_carries_its_wire_spelling() {
        // A renamed variant or a stray rename attribute would silently change
        // the request, so the value on the wire is asserted directly.
        for level in ThinkingLevel::ALL {
            let value = serde_json::to_value(level).expect("the level serialises");
            assert_eq!(value, serde_json::json!(level.wire()), "{level:?}");
        }
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
    fn a_user_turn_with_attachments_lists_their_paths() {
        let turn = UserTurn {
            text: "look".into(),
            attachments: vec![
                crate::attachments::Attachment {
                    path: "/abs/a.pdf".into(),
                    name: "a.pdf".into(),
                    bytes: 12,
                },
                crate::attachments::Attachment {
                    path: "/abs/b.png".into(),
                    name: "b.png".into(),
                    bytes: 34,
                },
            ],
        };
        let message = Message::user_turn(turn);
        let Some(MessageContent::Text(text)) = message.content else {
            panic!("attachments must stay a plain text body");
        };
        assert!(text.contains("look"), "{text}");
        assert!(text.contains("read_file"), "{text}");
        assert!(text.contains("read_image"), "{text}");
        assert!(text.contains("- /abs/a.pdf"), "{text}");
        assert!(text.contains("- /abs/b.png"), "{text}");
    }

    #[test]
    fn a_user_turn_without_attachments_is_just_the_text() {
        let message = Message::user_turn("hello".into());
        let value = serde_json::to_value(&message).unwrap();
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

    #[test]
    fn deepseek_reports_cache_hits_at_the_top_level() {
        let usage: Usage = serde_json::from_value(serde_json::json!({
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
        let usage: Usage = serde_json::from_value(serde_json::json!({
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
        let usage: Usage = serde_json::from_value(serde_json::json!({
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
}
