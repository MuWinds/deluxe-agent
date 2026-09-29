//! Adapting one MCP tool to the host's [`Tool`] trait.
//!
//! The server owns the tool: its name, its description and its argument schema
//! all come from `tools/list` and are passed through rather than rewritten. What
//! this module does is the two things the host requires of every tool — a
//! descriptor in the catalogue's shape, and a call that returns
//! [`ToolOutput`].
//!
//! # Two things worth knowing
//!
//! * The host does not validate the arguments. A server's JSON Schema can
//!   use keywords [`ObjectSchema`] cannot express, and the server validates the
//!   call itself, so [`ToolDescriptor::host_validates_arguments`] is `false`.
//!   What does not fit the narrow schema is appended to the description, where
//!   the model still reads it.
//! * An image a server returns is not shown. This host's image blocks are
//!   references to files on disk; MCP sends a picture inline as base64. Rather
//!   than drop it silently, the block is replaced by a line saying a picture
//!   came back, so the model knows the call produced one.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;
use crate::tools::{ContentBlock, ObjectSchema, Tool, ToolDescriptor, ToolOutput, ToolSettings};

use super::{SharedClient, ToolSpec};

/// How much of a description reaches the one-line summary.
///
/// The summary is what the system prompt lists, one bullet per tool, so it has
/// to stay short. The full description still reaches the model through the
/// tool's own definition.
const MAX_SUMMARY_CHARS: usize = 200;

/// The root keywords [`ObjectSchema`] models. Everything else a server's schema
/// declares has to travel in the description instead.
const CARRIED_KEYWORDS: [&str; 3] = ["type", "properties", "required"];

/// The name a tool is exposed under: `mcp__<server>__<tool>`.
///
/// The double-underscore convention is Codex's, and it earns its keep: an MCP
/// tool reads as obviously not a built-in in a transcript, and two servers that
/// both offer a `search` do not collide.
fn exposed_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// One of a server's tools, as the host's catalogue sees it.
pub struct McpTool {
    /// `mcp__<server>__<tool>` — the name the model calls.
    name: String,
    /// The server's own name for the tool, which is what `tools/call` wants.
    remote: String,
    summary: String,
    description: String,
    input_schema: ObjectSchema,
    client: SharedClient,
}

impl McpTool {
    /// Wraps one tool an MCP server advertised.
    ///
    /// The server's own schema is narrowed to this crate's [`ObjectSchema`]; any
    /// keyword it cannot express is folded into the description rather than
    /// dropped, so the model still learns the constraint.
    pub fn new(server: &str, spec: ToolSpec, client: SharedClient) -> Self {
        let (input_schema, leftover) = split_schema(&spec.input_schema);
        let description = describe(&spec, leftover.as_ref());
        let summary = first_line(&description, MAX_SUMMARY_CHARS);

        Self {
            name: exposed_name(server, &spec.name),
            remote: spec.name,
            summary,
            description,
            input_schema,
            client,
        }
    }

    /// The name this tool is registered and called under.
    ///
    /// Read before registering, so two servers offering the same tool name can
    /// be told apart instead of one silently replacing the other.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.clone(),
            summary: self.summary.clone(),
            description: self.description.clone(),
            // A server's tool contributes no host-wide rule: this host cannot
            // know what it does, and guessing would put a wrong instruction in
            // every prompt.
            guidelines: Vec::new(),
            input_schema: self.input_schema.clone(),
            // The server owns the schema and validates the call itself.
            host_validates_arguments: false,
            // MCP annotations are the server's hints, and nothing here gates on
            // `mutating`; calling a tool read-only on a hint would be a claim
            // this host cannot back.
            mutating: true,
        }
    }

    async fn execute(&self, arguments: Value, _settings: &ToolSettings) -> Result<ToolOutput> {
        // The per-call timeout in `Agent::dispatch` bounds this, so a server
        // that stops answering does not hang the run.
        let result = self
            .client
            .lock()
            .await
            .call_tool(&self.remote, &arguments)
            .await?;
        Ok(render_result(result))
    }
}

/// Splits a server's JSON Schema into what the host can carry and what it
/// cannot.
///
/// [`ObjectSchema`] is deliberately narrow — `type`, `properties`, `required` —
/// because that is all a hand-written built-in needs. A server's schema is
/// richer: `$defs`, `additionalProperties`, a root-level `oneOf`. Each
/// property's own subschema survives verbatim, because `properties` holds
/// arbitrary values; it is the root keywords that do not fit.
fn split_schema(raw: &Value) -> (ObjectSchema, Option<Value>) {
    let schema = ObjectSchema {
        schema_type: raw
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("object")
            .to_string(),
        properties: raw
            .get("properties")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        required: raw
            .get("required")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    };

    let leftover = raw.as_object().and_then(|map| {
        let rest: serde_json::Map<String, Value> = map
            .iter()
            .filter(|(key, _)| !CARRIED_KEYWORDS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        (!rest.is_empty()).then_some(Value::Object(rest))
    });

    (schema, leftover)
}

/// The tool's description, with whatever the host's schema cannot express
/// appended so the model still sees it.
fn describe(spec: &ToolSpec, leftover: Option<&Value>) -> String {
    let mut description = match spec.description.as_deref() {
        Some(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => "This tool's server gave no description.".to_string(),
    };

    if let Some(leftover) = leftover {
        description.push_str(
            "\n\nThe server's schema also declares the following, which the host's tool \
             schema cannot carry:\n",
        );
        description.push_str(
            &serde_json::to_string_pretty(leftover).unwrap_or_else(|_| leftover.to_string()),
        );
    }
    description
}

/// The first line of `text`, capped on a character boundary so a multi-byte
/// character is never split.
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut capped: String = line.chars().take(max).collect();
    capped.push('…');
    capped
}

/// Turns an MCP `tools/call` result into host tool output.
///
/// A block the host cannot render becomes a line saying what it was, rather
/// than being dropped: a call that returned a picture and a call that returned
/// nothing should not look the same to the model.
fn render_result(result: Value) -> ToolOutput {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut content = Vec::new();
    for block in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    content.push(ContentBlock::text(text));
                }
            }
            Some("image") => {
                let media = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type");
                content.push(ContentBlock::text(format!(
                    "[the server returned an image ({media}); this host cannot display a \
                     picture that arrives inside a tool result]"
                )));
            }
            // A resource carries either its own text or a pointer to one. The
            // text is worth passing on; the pointer is worth naming, so the
            // model can decide to read it with `read_file`.
            Some("resource") => {
                let resource = block.get("resource");
                match resource
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                {
                    Some(text) => content.push(ContentBlock::text(text)),
                    None => {
                        let uri = resource
                            .and_then(|value| value.get("uri"))
                            .and_then(Value::as_str)
                            .unwrap_or("no uri");
                        content.push(ContentBlock::text(format!(
                            "[a resource the server returned: {uri}]"
                        )));
                    }
                }
            }
            Some(kind) => content.push(ContentBlock::text(format!(
                "[a `{kind}` content block, which this host does not display]"
            ))),
            None => {}
        }
    }

    // The structured half of a result, when the server sends one. Appended
    // rather than preferred: it is the same answer in a second shape, and the
    // model reads prose better than JSON.
    if let Some(structured) = result.get("structuredContent") {
        if !structured.is_null() {
            content.push(ContentBlock::text(format!(
                "\n{}",
                serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string())
            )));
        }
    }

    if content.is_empty() {
        content.push(ContentBlock::text("The tool returned no content."));
    }

    ToolOutput {
        content,
        is_error,
        truncated: false,
        original_bytes: None,
        duration_ms: None,
        hunks: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{shared, McpClient};
    use serde_json::json;

    /// A client nothing is ever sent to, so a tool can be built and inspected
    /// without a server to talk to.
    fn unused_client() -> SharedClient {
        let (client, server) = tokio::io::duplex(64);
        drop(server);
        shared(McpClient::for_test(client))
    }

    /// A tool built from the `tools/list` shape, so the tests read like the
    /// wire without `ToolSpec` needing a `Deserialize` impl it has no other use
    /// for.
    fn tool(listed: Value) -> McpTool {
        let spec = ToolSpec {
            name: listed["name"].as_str().unwrap_or("unnamed").to_string(),
            description: listed
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            input_schema: listed
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
        };
        McpTool::new("figma", spec, unused_client())
    }

    #[test]
    fn the_exposed_name_namespaces_the_server_and_the_tool() {
        let built = tool(json!({"name": "search"}));
        assert_eq!(built.name(), "mcp__figma__search");
    }

    #[test]
    fn a_servers_schema_is_carried_as_far_as_the_host_schema_can_take_it() {
        let built = tool(json!({
            "name": "search",
            "description": "Find things",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "q": {"type": "string", "enum": ["a", "b"]},
                    "nested": {"items": {"$ref": "#/$defs/thing"}},
                },
                "required": ["q"],
            },
        }));
        let descriptor = built.descriptor();

        assert_eq!(descriptor.name, "mcp__figma__search");
        assert_eq!(descriptor.input_schema.schema_type, "object");
        assert_eq!(descriptor.input_schema.required, vec!["q"]);
        assert_eq!(
            descriptor.input_schema.properties["q"]["enum"],
            json!(["a", "b"]),
            "a property's own subschema survives verbatim"
        );
        assert_eq!(
            descriptor.input_schema.properties["nested"]["items"]["$ref"], "#/$defs/thing",
            "including a reference the narrow schema cannot resolve"
        );
    }

    #[test]
    fn the_host_does_not_validate_a_servers_arguments() {
        // The server owns the schema, and `ObjectSchema` cannot express all of
        // it — so the host must not be the one to refuse a call.
        let built = tool(json!({"name": "t", "inputSchema": {"type": "object"}}));
        assert!(!built.descriptor().host_validates_arguments);
    }

    #[test]
    fn root_keywords_the_host_schema_cannot_hold_reach_the_description() {
        let built = tool(json!({
            "name": "t",
            "description": "Does a thing.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false,
                "$defs": {"thing": {"type": "string"}},
            },
        }));
        let description = built.descriptor().description;

        assert!(description.starts_with("Does a thing."), "{description}");
        assert!(
            description.contains("additionalProperties"),
            "the leftover keywords are shown to the model rather than dropped: {description}"
        );
        assert!(description.contains("$defs"), "{description}");
        assert!(
            !description.contains("\"required\""),
            "a keyword the host schema carries is not repeated: {description}"
        );
    }

    #[test]
    fn a_schema_that_fits_is_not_duplicated_into_the_description() {
        let built = tool(json!({
            "name": "t",
            "description": "Does a thing.",
            "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}},
        }));

        assert_eq!(built.descriptor().description, "Does a thing.");
    }

    #[test]
    fn a_tool_with_no_description_still_gets_one() {
        let built = tool(json!({"name": "t"}));
        assert_eq!(
            built.descriptor().description,
            "This tool's server gave no description."
        );
        assert_eq!(
            built.descriptor().summary,
            "This tool's server gave no description."
        );
    }

    #[test]
    fn the_summary_is_the_first_line_capped() {
        let built = tool(json!({
            "name": "t",
            "description": format!("{}\nand a second line", "x".repeat(300)),
        }));

        let summary = built.descriptor().summary;
        assert_eq!(
            summary.chars().count(),
            MAX_SUMMARY_CHARS + 1,
            "capped plus the ellipsis"
        );
        assert!(summary.ends_with('…'));
        assert!(!summary.contains("second line"));
    }

    #[test]
    fn text_blocks_become_the_tool_output() {
        let output = render_result(json!({
            "content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
        }));

        assert_eq!(output.as_text(), "onetwo");
        assert!(!output.is_error);
    }

    #[test]
    fn an_error_result_is_an_error() {
        let output = render_result(json!({
            "isError": true,
            "content": [{"type": "text", "text": "it broke"}],
        }));

        assert!(output.is_error);
        assert_eq!(output.as_text(), "it broke");
    }

    #[test]
    fn an_image_is_reported_rather_than_dropped() {
        let output = render_result(json!({
            "content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}],
        }));

        let text = output.as_text();
        assert!(text.contains("image/png"), "{text}");
        assert!(text.contains("cannot display"), "{text}");
    }

    #[test]
    fn a_text_resource_is_passed_through_and_a_pointer_is_named() {
        let with_text = render_result(json!({
            "content": [{"type": "resource", "resource": {"uri": "file:///a", "text": "body"}}],
        }));
        assert_eq!(with_text.as_text(), "body");

        let pointer = render_result(json!({
            "content": [{"type": "resource", "resource": {"uri": "file:///a"}}],
        }));
        assert!(
            pointer.as_text().contains("file:///a"),
            "{}",
            pointer.as_text()
        );
    }

    #[test]
    fn an_empty_result_says_so_rather_than_reading_as_blank() {
        let output = render_result(json!({"content": []}));
        assert_eq!(output.as_text(), "The tool returned no content.");
    }

    #[test]
    fn structured_content_is_appended() {
        let output = render_result(json!({
            "content": [{"type": "text", "text": "summary"}],
            "structuredContent": {"count": 2},
        }));

        let text = output.as_text();
        assert!(text.starts_with("summary"), "{text}");
        assert!(text.contains("\"count\""), "{text}");
    }
}
