//! The wire protocol: request/response envelopes and the two entry points.
//!
//! The host validates every response against the same schema before it reaches
//! the UI, so this module only needs to shape the JSON — it is not the trust
//! boundary.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::code_view::{self, ToolOutcome};
use crate::display::Node;
use crate::markdown;

/// Bumped only when the request/response shape changes incompatibly.
pub const SCHEMA_VERSION: u32 = 2;

/// The content width the renderer may lay out into, plus the two measurements
/// it cannot take itself.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub available_width: f32,
    pub char_width: f32,
    pub column_gap: f32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRequest {
    #[serde(default)]
    pub revision: u64,
    pub text: String,
    pub metrics: Metrics,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    schema_version: u32,
    revision: u64,
    kind: &'static str,
    nodes: Vec<Node>,
}

/// Parses a message request and returns its display-list response.
///
/// Returns `Err` only when the request is malformed; the parse itself never
/// fails (an unterminated fence is normal mid-stream).
pub fn render_message(request_json: &str) -> Result<String, String> {
    let request: MessageRequest =
        serde_json::from_str(request_json).map_err(|error| error.to_string())?;
    let nodes = markdown::to_display(&markdown::parse(&request.text), &request.metrics);
    to_json(request.revision, "message", nodes)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequest {
    #[serde(default)]
    pub revision: u64,
    pub metrics: Metrics,
    pub tool: ToolInput,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInput {
    pub name: String,
    /// The parsed arguments object; `null` when the call had none.
    #[serde(default)]
    pub arguments: Value,
    /// `null` while the call is still running.
    #[serde(default)]
    pub result: Option<ToolOutcome>,
}

/// Shapes a tool request into its display-list response.
///
/// Returns `Err` only when the request is malformed.
pub fn render_tool(request_json: &str) -> Result<String, String> {
    let request: ToolRequest =
        serde_json::from_str(request_json).map_err(|error| error.to_string())?;
    let nodes = code_view::build_display(
        &request.tool.name,
        &request.tool.arguments,
        request.tool.result.as_ref(),
        &request.metrics,
    );
    to_json(request.revision, "tool", nodes)
}

fn to_json(revision: u64, kind: &'static str, nodes: Vec<Node>) -> Result<String, String> {
    let response = Response {
        schema_version: SCHEMA_VERSION,
        revision,
        kind,
        nodes,
    };
    serde_json::to_string(&response).map_err(|error| error.to_string())
}
