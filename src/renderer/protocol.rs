//! The transcript renderer's display list and wire protocol.
//!
//! The renderer Component turns a message body or a tool call into a tree of
//! generic presentation nodes — styled text runs, stacks, indents, frames,
//! rules, scroll areas, folds, icons, copy buttons, alignment boxes and grids.
//! The host walks that tree with `crate::renderer::present` and never learns
//! what a heading, a table or a patch is: all of that shaping happens in Wasm.
//!
//! Every response is decoded and bounded here before it reaches the UI. This
//! module is deliberately independent of egui and of Wasmtime — it is the
//! contract, not an implementation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::{code, AgentError, Result};

/// The request/response shape version. Bumped only on an incompatible change.
pub const SCHEMA_VERSION: u32 = 2;

/// The most nodes one display list may contain.
pub const MAX_NODES: usize = 16 * 1024;
/// The deepest a display list may nest.
pub const MAX_NODE_DEPTH: usize = 32;
/// The most runs one text node may contain.
pub const MAX_RUNS_PER_TEXT: usize = 4096;
/// The most columns a grid may declare.
pub const MAX_GRID_COLUMNS: usize = 64;
/// The most rows a grid may carry.
pub const MAX_GRID_ROWS: usize = 4096;
/// The longest a single text field may be.
pub const MAX_TEXT_FIELD_BYTES: usize = 16 * 1024;
/// The largest total text one response may carry.
pub const MAX_AGGREGATE_TEXT_BYTES: usize = 512 * 1024;
/// The longest a link URL may be.
pub const MAX_LINK_BYTES: usize = 4 * 1024;

/// Identifies the transcript item a render request belongs to.
///
/// Stable across a step's streaming lifetime: text grows, the key does not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RenderKey {
    /// An assistant, notice, host, or reasoning message body.
    Message { session: Uuid, step: usize },
    /// A tool card, keyed by the model's call id.
    Tool { session: Uuid, call_id: String },
}

/// Which renderer entry point a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderKind {
    Message,
    Tool,
}

impl RenderKind {
    /// The response `kind` string this entry point must echo back.
    pub fn as_str(self) -> &'static str {
        match self {
            RenderKind::Message => "message",
            RenderKind::Tool => "tool",
        }
    }
}

/// A semantic colour a node asks for; the host resolves it against its palette.
///
/// Roles, not values: the renderer never names a colour, so the theme stays a
/// host concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ColorRole {
    Text,
    Muted,
    /// Muted ink for a secondary hint, dimmer than [`ColorRole::Muted`].
    Dim,
    Accent,
    Success,
    Warning,
    Danger,
    DiffAddFg,
    DiffDelFg,
    DiffAddBg,
    DiffDelBg,
    CodeBg,
    PanelBg,
    PanelHeaderBg,
    Border,
}

/// A font family a run asks for; the host resolves it and applies the theme's
/// font scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FontRole {
    Proportional,
    Monospace,
}

/// Horizontal placement of a box or a grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HorizontalAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Vertical placement of the children of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerticalAlign {
    #[default]
    Top,
    Center,
}

/// Per-corner radii of a frame, in design units.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CornerSpec {
    pub nw: f32,
    pub ne: f32,
    pub sw: f32,
    pub se: f32,
}

/// A frame's inset on each side, in design units.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeInsets {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

/// A one-side border, painted over a frame's edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Border {
    pub width: f32,
    pub color: ColorRole,
}

/// One styled run of text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub text: String,
    pub font: FontRole,
    /// Design units; the host applies the theme's font scale.
    pub size: f32,
    pub color: ColorRole,
    /// `Some(role)` fills the run's own box behind the glyphs — a diff tint or
    /// an inline code chip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<ColorRole>,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    /// `Some(url)` makes this run a clickable link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// `Some(kind)` prefixes the run with an icon glyph the host resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconKind>,
}

impl Run {
    /// A plain proportional run, for tests that build a display list by hand.
    #[cfg(test)]
    pub fn text(text: impl Into<String>, size: f32, color: ColorRole) -> Self {
        Self {
            text: text.into(),
            font: FontRole::Proportional,
            size,
            color,
            background: None,
            italic: false,
            underline: false,
            link: None,
            icon: None,
        }
    }
}

/// One column of a grid: its placement and its width in design units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GridColumn {
    pub align: HorizontalAlign,
    pub width: f32,
}

/// One node of the display list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Node {
    /// A line of styled runs.
    Text {
        runs: Vec<Run>,
        #[serde(default)]
        wrap: bool,
        #[serde(default = "default_true")]
        selectable: bool,
    },
    /// A vertical stack.
    Column { children: Vec<Node> },
    /// A horizontal stack.
    Row {
        children: Vec<Node>,
        #[serde(default)]
        gap: f32,
        #[serde(default)]
        align: VerticalAlign,
    },
    /// Left padding in front of a child.
    Indent { amount: f32, child: Box<Node> },
    /// A filled, stroked, or one-side-bordered container.
    Frame {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fill: Option<ColorRole>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stroke: Option<ColorRole>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left_bar: Option<Border>,
        #[serde(default)]
        radius: CornerSpec,
        #[serde(default)]
        padding: EdgeInsets,
        /// When true the frame fills the available width; else it shrinks.
        #[serde(default)]
        stretch: bool,
        child: Box<Node>,
    },
    /// A full-width hairline.
    Rule,
    /// Fixed vertical space.
    Spacer { size: f32 },
    /// A flexible spacer inside a row.
    Grow,
    /// A scroll area; long content is scrolled rather than grown.
    Scroll {
        max_height: f32,
        #[serde(default)]
        both_axes: bool,
        child: Box<Node>,
    },
    /// A fold whose header is a single line of runs.
    Collapse {
        header: Vec<Run>,
        body: Vec<Node>,
        #[serde(default)]
        default_open: bool,
        #[serde(default = "default_true")]
        show_background: bool,
    },
    /// An icon glyph the host resolves.
    Icon {
        kind: IconKind,
        color: ColorRole,
        size: f32,
    },
    /// A button that copies its text to the clipboard.
    CopyButton { text: String, hover: String },
    /// Places its child on one side of the available width.
    Align {
        align: HorizontalAlign,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
        child: Box<Node>,
    },
    /// A fixed-width grid of cells.
    Grid {
        columns: Vec<GridColumn>,
        #[serde(default)]
        gap: f32,
        header: Vec<Vec<Run>>,
        rows: Vec<Vec<Vec<Run>>>,
        #[serde(default = "default_true")]
        rules: bool,
    },
}

fn default_true() -> bool {
    true
}

/// The semantic glyph a node or run asks for; the host maps it to a glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IconKind {
    NotePencil,
    Terminal,
    File,
    Folder,
    Image,
    Dots,
    StopCircle,
    Gear,
    CheckCircle,
    WarningCircle,
    XCircle,
    Spinner,
}

/// The content width the renderer may lay out into, plus the two measurements
/// it cannot take itself.
///
/// The renderer has no fonts, so the host passes the numbers it would otherwise
/// have to guess: the width available for content, and the advance of one
/// average character at the body font.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderMetrics {
    pub available_width: f32,
    pub char_width: f32,
    pub column_gap: f32,
}

/// The host's projection of one tool call onto the wire.
///
/// A dedicated shape rather than the host's `ToolResult`: images never cross the
/// boundary, and the patch line table is flattened to what the panel needs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRenderRequest {
    pub schema_version: u32,
    pub revision: u64,
    pub metrics: RenderMetrics,
    pub tool: ToolRenderInput,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRenderInput {
    pub name: String,
    pub arguments: Value,
    /// `None` while the call is still running.
    pub result: Option<ToolRenderResult>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRenderResult {
    /// `executed`, `denied`, or `failed`.
    pub outcome: String,
    pub output: String,
    pub hunks: Vec<HunkLines>,
    /// Wall-clock duration, shown on the fold's header.
    pub duration_ms: u64,
}

/// One file section of the execution-time line-number table.
#[derive(Debug, Clone, Serialize)]
pub struct HunkLines {
    pub path: String,
    pub lines: Vec<Option<usize>>,
}

impl ToolRenderRequest {
    /// Builds a request for one tool call.
    pub fn new(
        revision: u64,
        name: impl Into<String>,
        arguments: Value,
        result: Option<ToolRenderResult>,
        metrics: RenderMetrics,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            revision,
            metrics,
            tool: ToolRenderInput {
                name: name.into(),
                arguments,
                result,
            },
        }
    }
}

/// Builds a `render-message` request body.
pub fn message_request(revision: u64, text: &str, metrics: RenderMetrics) -> String {
    serde_json::json!({
        "schemaVersion": SCHEMA_VERSION,
        "revision": revision,
        "text": text,
        "metrics": metrics,
    })
    .to_string()
}

/// A decoded display-list response, before validation.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderResponse {
    pub schema_version: u32,
    pub revision: u64,
    pub kind: String,
    pub nodes: Vec<Node>,
}

/// Decodes and validates a display-list response.
///
/// Returns `Err` for malformed JSON, a version/revision/kind mismatch, or a
/// tree that exceeds any protocol budget.
pub fn decode(json: &str, revision: u64, expected: RenderKind) -> Result<Vec<Node>> {
    let response: RenderResponse =
        serde_json::from_str(json).map_err(|error| invalid(format!("render JSON: {error}")))?;
    if response.schema_version != SCHEMA_VERSION
        || response.revision != revision
        || response.kind != expected.as_str()
    {
        return Err(invalid("renderer response identity is invalid"));
    }
    let mut state = Budget::default();
    for node in &response.nodes {
        validate_node(node, 1, &mut state)?;
    }
    Ok(response.nodes)
}

/// The running totals a validation walk enforces.
#[derive(Default)]
struct Budget {
    nodes: usize,
    text: usize,
}

fn check_text(value: &str) -> Result<()> {
    if value.len() > MAX_TEXT_FIELD_BYTES {
        return Err(invalid("renderer text field exceeds its limit"));
    }
    Ok(())
}

fn check_finite(value: f32) -> Result<()> {
    if !value.is_finite() {
        return Err(invalid("renderer emitted a non-finite measurement"));
    }
    Ok(())
}

/// Validates one node and everything under it, tracking depth and the running
/// node and text totals.
fn validate_node(node: &Node, depth: usize, budget: &mut Budget) -> Result<()> {
    if depth > MAX_NODE_DEPTH {
        return Err(invalid("display list nests too deeply"));
    }
    budget.nodes += 1;
    if budget.nodes > MAX_NODES {
        return Err(invalid("display list has too many nodes"));
    }
    match node {
        Node::Text { runs, .. } => validate_runs(runs, budget)?,
        Node::Column { children } => {
            for child in children {
                validate_node(child, depth + 1, budget)?;
            }
        }
        Node::Row { children, gap, .. } => {
            check_finite(*gap)?;
            for child in children {
                validate_node(child, depth + 1, budget)?;
            }
        }
        Node::Indent { amount, child } => {
            check_finite(*amount)?;
            validate_node(child, depth + 1, budget)?;
        }
        Node::Frame {
            radius,
            padding,
            left_bar,
            child,
            ..
        } => {
            for value in [radius.nw, radius.ne, radius.sw, radius.se] {
                check_finite(value)?;
            }
            for value in [padding.left, padding.right, padding.top, padding.bottom] {
                check_finite(value)?;
            }
            if let Some(bar) = left_bar {
                check_finite(bar.width)?;
            }
            validate_node(child, depth + 1, budget)?;
        }
        Node::Rule | Node::Grow => {}
        Node::Spacer { size } => check_finite(*size)?,
        Node::Scroll {
            max_height, child, ..
        } => {
            check_finite(*max_height)?;
            validate_node(child, depth + 1, budget)?;
        }
        Node::Collapse { header, body, .. } => {
            validate_runs(header, budget)?;
            for child in body {
                validate_node(child, depth + 1, budget)?;
            }
        }
        Node::Icon { size, .. } => check_finite(*size)?,
        Node::CopyButton { text, hover } => {
            check_text(text)?;
            check_text(hover)?;
            budget.text += text.len() + hover.len();
            check_aggregate(budget)?;
        }
        Node::Align { width, child, .. } => {
            if let Some(width) = width {
                check_finite(*width)?;
            }
            validate_node(child, depth + 1, budget)?;
        }
        Node::Grid {
            columns,
            gap,
            header,
            rows,
            ..
        } => {
            check_finite(*gap)?;
            if columns.len() > MAX_GRID_COLUMNS || rows.len() > MAX_GRID_ROWS {
                return Err(invalid("grid is too large"));
            }
            for column in columns {
                check_finite(column.width)?;
                if column.width <= 0.0 {
                    return Err(invalid("grid column width must be positive"));
                }
            }
            if header.len() != columns.len() {
                return Err(invalid("grid header does not match its columns"));
            }
            validate_runs(header.iter().flatten(), budget)?;
            for row in rows {
                if row.len() != columns.len() {
                    return Err(invalid("grid row does not match its columns"));
                }
                validate_runs(row.iter().flatten(), budget)?;
            }
        }
    }
    Ok(())
}

fn validate_runs<'a>(runs: impl IntoIterator<Item = &'a Run>, budget: &mut Budget) -> Result<()> {
    let mut count = 0usize;
    for run in runs {
        count += 1;
        if count > MAX_RUNS_PER_TEXT {
            return Err(invalid("text node has too many runs"));
        }
        check_finite(run.size)?;
        check_text(&run.text)?;
        budget.text += run.text.len();
        if let Some(link) = &run.link {
            validate_link(link)?;
            budget.text += link.len();
        }
        check_aggregate(budget)?;
    }
    Ok(())
}

fn check_aggregate(budget: &Budget) -> Result<()> {
    if budget.text > MAX_AGGREGATE_TEXT_BYTES {
        return Err(invalid("display list text exceeds the aggregate limit"));
    }
    Ok(())
}

/// Rejects links that are oversized, carry control characters, or name a scheme
/// the host will not open. A scheme-less URL is allowed.
fn validate_link(url: &str) -> Result<()> {
    if url.len() > MAX_LINK_BYTES || url.chars().any(char::is_control) {
        return Err(invalid("link is invalid"));
    }
    if let Some(colon) = url.find(':') {
        let scheme = &url[..colon];
        if !scheme.is_empty() && !url[..colon].contains('/') {
            let known = ["http", "https", "mailto"]
                .iter()
                .any(|allowed| scheme.eq_ignore_ascii_case(allowed));
            if !known {
                return Err(invalid("link scheme is not allowed"));
            }
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::new(code::RENDER_INVALID_OUTPUT, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(revision: u64, kind: &str, nodes: &str) -> String {
        format!(r#"{{"schemaVersion":2,"revision":{revision},"kind":"{kind}","nodes":[{nodes}]}}"#)
    }

    fn text_node(text: &str, link: &str) -> String {
        format!(
            r#"{{"type":"text","runs":[{{"text":{text:?},"font":"proportional","size":14.0,"color":"text","link":{link}}}]}}"#
        )
    }

    #[test]
    fn a_valid_message_response_decodes() {
        let json = response(7, "message", &text_node("hi", "null"));
        let nodes = decode(&json, 7, RenderKind::Message).expect("valid response decodes");
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            Node::Text { runs, .. } => assert_eq!(runs[0].text, "hi"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn revision_schema_and_kind_must_match() {
        let json = response(7, "message", &text_node("hi", "null"));
        assert!(
            decode(&json, 8, RenderKind::Message).is_err(),
            "revision mismatch"
        );
        assert!(decode(&json, 7, RenderKind::Tool).is_err(), "kind mismatch");
        let old = r#"{"schemaVersion":1,"revision":7,"kind":"message","nodes":[]}"#;
        assert!(
            decode(old, 7, RenderKind::Message).is_err(),
            "version mismatch"
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{"schemaVersion":2,"revision":7,"kind":"message","future":true,"nodes":[]}"#;
        assert!(decode(json, 7, RenderKind::Message).is_ok());
    }

    #[test]
    fn oversized_text_and_links_are_rejected() {
        let big = "x".repeat(MAX_TEXT_FIELD_BYTES + 1);
        let json = response(7, "message", &text_node(&big, "null"));
        assert!(decode(&json, 7, RenderKind::Message).is_err());

        let long_link = "https://".to_string() + &"x".repeat(MAX_LINK_BYTES);
        let json = response(7, "message", &text_node("hi", &format!("{long_link:?}")));
        assert!(decode(&json, 7, RenderKind::Message).is_err());
    }

    #[test]
    fn node_depth_and_count_are_bounded() {
        let deep = (0..(MAX_NODE_DEPTH + 2)).fold(text_node("hi", "null"), |inner, _| {
            format!(r#"{{"type":"column","children":[{inner}]}}"#)
        });
        let json = response(7, "message", &deep);
        assert!(decode(&json, 7, RenderKind::Message).is_err());
    }

    #[test]
    fn a_non_rectangular_grid_is_rejected() {
        let grid = r#"{"type":"grid","columns":[{"align":"left","width":10.0},{"align":"left","width":10.0}],"header":[[{"text":"a","font":"proportional","size":12.0,"color":"text"}]],"rows":[]}"#;
        let json = response(7, "message", grid);
        assert!(decode(&json, 7, RenderKind::Message).is_err());

        let bad_width =
            r#"{"type":"grid","columns":[{"align":"left","width":0.0}],"header":[[]],"rows":[]}"#;
        let json = response(7, "message", bad_width);
        assert!(decode(&json, 7, RenderKind::Message).is_err());
    }

    #[test]
    fn non_finite_geometry_is_rejected() {
        let mut budget = Budget::default();
        assert!(validate_node(&Node::Spacer { size: f32::NAN }, 1, &mut budget).is_err());

        let mut budget = Budget::default();
        let bad = Node::Text {
            runs: vec![Run::text("hi", f32::INFINITY, ColorRole::Text)],
            wrap: false,
            selectable: true,
        };
        assert!(validate_node(&bad, 1, &mut budget).is_err());
    }

    #[test]
    fn link_schemes_are_bounded() {
        assert!(validate_link("https://example.com").is_ok());
        assert!(validate_link("/relative/path").is_ok());
        assert!(validate_link("javascript:alert(1)").is_err());
        assert!(validate_link(&format!("https://{}", "x".repeat(MAX_LINK_BYTES))).is_err());
    }
}
