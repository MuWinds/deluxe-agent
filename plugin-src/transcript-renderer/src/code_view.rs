//! Tool result → display list.
//!
//! The shaping half of the old `code_view`: it turns a tool name, its arguments
//! and its result into the generic nodes the host draws. Every visual decision
//! — the fold, the title bar, the diff tint, the gutter, the copy button — is
//! made here, so the host never learns what a patch or an `exec` is.

use serde::Deserialize;
use serde_json::Value;

use crate::display::{ColorRole, CornerSpec, EdgeInsets, IconKind, Node, Run, VerticalAlign};
use crate::protocol::Metrics;

/// The tallest a panel body grows before it scrolls.
const PANEL_MAX_HEIGHT: f32 = 320.0;
const PANEL_RADIUS: f32 = 10.0;
const CODE_SIZE: f32 = 12.0;
const TITLE_SIZE: f32 = 12.0;
/// The two-column marker that stands in for a gutter.
const MARKER_WIDTH: &str = "  ";

/// What a line of a panel means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged context, or plain output.
    Plain,
    /// A line the call introduced.
    Add,
    /// A line the call removed.
    Del,
    /// A patch hunk boundary.
    Hunk,
    /// Structural text: a file header inside a patch, or a shell prompt.
    Meta,
}

/// A line of a panel body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
    /// The line's position in the file it edits, one-based, or `None` for
    /// scaffolding.
    pub num: Option<usize>,
}

impl Line {
    fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            num: None,
        }
    }

    fn numbered(kind: LineKind, text: impl Into<String>, num: usize) -> Self {
        Self {
            kind,
            text: text.into(),
            num: Some(num),
        }
    }
}

/// Whether this build knows how to shape the call's body.
pub fn is_known(name: &str) -> bool {
    matches!(
        name,
        "apply_patch"
            | "exec"
            | "read_file"
            | "list_dir"
            | "read_image"
            | "job_output"
            | "job_list"
            | "job_kill"
    )
}

/// The semantic icon for a tool.
pub fn tool_icon(name: &str) -> IconKind {
    match name {
        "apply_patch" => IconKind::NotePencil,
        "exec" => IconKind::Terminal,
        "read_file" => IconKind::File,
        "list_dir" => IconKind::Folder,
        "read_image" => IconKind::Image,
        "job_output" => IconKind::Terminal,
        "job_list" => IconKind::Dots,
        "job_kill" => IconKind::StopCircle,
        _ => IconKind::Gear,
    }
}

/// The verb a collapsed row shows, which reads better than the tool's own name.
pub fn tool_label(name: &str) -> &str {
    match name {
        "apply_patch" => "编辑了文件",
        "exec" => "运行了命令",
        "read_file" => "读取了文件",
        "list_dir" => "列出了目录",
        "read_image" => "查看了图片",
        "job_output" => "读取了后台任务",
        "job_list" => "列出了后台任务",
        "job_kill" => "停止了后台任务",
        other => other,
    }
}

/// Splits a `*** Begin Patch` document into typed lines.
pub fn patch_lines(patch: &str, numbers: Option<&[Option<usize>]>) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line_no = 0usize;
    let mut next_add = 1usize;
    let mut computed = numbers.into_iter().flatten();
    for raw in patch.lines() {
        if raw == "*** Begin Patch" || raw == "*** End Patch" {
            continue;
        }
        if raw.starts_with("*** ") {
            if raw.starts_with("*** Add File: ") || raw.starts_with("*** Update File: ") {
                line_no = 0;
                next_add = 1;
            }
            lines.push(Line::new(LineKind::Meta, raw));
            continue;
        }
        if raw.starts_with("@@") {
            lines.push(Line::new(LineKind::Hunk, raw));
            continue;
        }
        let (kind, text) = match raw.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &raw[1..]),
            Some(b'-') => (LineKind::Del, &raw[1..]),
            Some(b' ') => (LineKind::Plain, &raw[1..]),
            _ => (LineKind::Plain, raw),
        };
        match raw.as_bytes().first() {
            Some(b'+') | Some(b'-') | Some(b' ') => {
                let num = match computed.next().copied().flatten() {
                    Some(num) => {
                        line_no = num;
                        next_add = match raw.as_bytes().first() {
                            Some(b'-') => num,
                            _ => num + 1,
                        };
                        num
                    }
                    None => match raw.as_bytes().first() {
                        Some(b'+') => {
                            let num = next_add;
                            next_add = num + 1;
                            num
                        }
                        _ => {
                            line_no += 1;
                            let num = line_no;
                            next_add = match raw.as_bytes().first() {
                                Some(b'-') => num,
                                _ => num + 1,
                            };
                            num
                        }
                    },
                };
                lines.push(Line::numbered(kind, text, num));
            }
            _ => {
                computed.next();
                lines.push(Line::new(kind, text));
            }
        }
    }
    lines
}

/// How many lines the patch adds and removes.
pub fn patch_stats(patch: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in patch_lines(patch, None) {
        match line.kind {
            LineKind::Add => added += 1,
            LineKind::Del => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

/// What a patch touches, for the title bar.
pub fn patch_title(patch: &str) -> String {
    let mut files: Vec<&str> = Vec::new();
    for raw in patch.lines() {
        let path = raw
            .strip_prefix("*** Add File: ")
            .or_else(|| raw.strip_prefix("*** Update File: "))
            .or_else(|| raw.strip_prefix("*** Delete File: "));
        if let Some(path) = path {
            let path = path.trim();
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    match files.as_slice() {
        [] => "补丁".to_string(),
        [only] => (*only).to_string(),
        many => format!("{} 个文件", many.len()),
    }
}

pub fn text_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|line| Line::new(LineKind::Plain, line))
        .collect()
}

/// Text that is part of the panel's scaffolding rather than its payload.
pub fn meta_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|line| Line::new(LineKind::Meta, line))
        .collect()
}

/// An `exec` panel: the command as a prompt line, then whatever it printed.
pub fn command_lines(command: &str, output: &str) -> Vec<Line> {
    let mut lines = vec![Line::new(LineKind::Meta, format!("$ {command}"))];
    lines.extend(text_lines(output));
    lines
}

/// Flattens to one line and caps it, counting characters rather than bytes.
fn shorten(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max).collect();
    format!("{head}…")
}

/// The one-line gist of a call, shown beside its verb on the collapsed row.
fn summarize(name: &str, arguments: &Value) -> Option<String> {
    let text = match name {
        "exec" => arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|command| format!("$ {command}")),
        "read_file" | "list_dir" => arguments
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string),
        "apply_patch" => arguments
            .get("patch")
            .and_then(Value::as_str)
            .map(patch_title),
        _ => None,
    };
    text.map(|text| shorten(&text, 200))
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// One file section of the execution-time line-number table.
#[derive(Debug, Clone, Deserialize)]
pub struct HunkSection {
    pub lines: Vec<Option<usize>>,
}

/// A tool call's result as the host projects it onto the wire.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutcome {
    /// `executed`, `denied`, or `failed`.
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub hunks: Vec<HunkSection>,
    #[serde(default)]
    pub duration_ms: u64,
}

/// Shapes a tool call into the display list the host draws.
///
/// The root is a fold: its header is the collapsed row, its body the panel (and
/// the raw-arguments fold, for a tool this build does not know).
pub fn build_display(
    name: &str,
    arguments: &Value,
    result: Option<&ToolOutcome>,
    _metrics: &Metrics,
) -> Vec<Node> {
    let running = result.is_none();
    let output = result.map(|result| result.output.as_str()).unwrap_or_default();
    let icon = tool_icon(name);

    let (title, added, removed, copy, lines) = match name {
        "apply_patch" => {
            let patch = arguments
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (added, removed) = patch_stats(patch);
            let numbers = result.map(|result| {
                result
                    .hunks
                    .iter()
                    .flat_map(|section| section.lines.iter().copied())
                    .collect::<Vec<_>>()
            });
            let mut lines = patch_lines(patch, numbers.as_deref());
            lines.extend(meta_lines(output));
            (
                shorten(&patch_title(patch), 80),
                added,
                removed,
                patch.to_string(),
                lines,
            )
        }
        "exec" => (
            "Shell".to_string(),
            0,
            0,
            output.to_string(),
            command_lines(
                arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                output,
            ),
        ),
        _ => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(name);
            (
                shorten(path, 80),
                0,
                0,
                output.to_string(),
                text_lines(output),
            )
        }
    };

    let accent = accent_role(result);
    let panel = panel_node(&Panel {
        title: &title,
        icon,
        added,
        removed,
        copy: &copy,
        lines: &lines,
        running,
        accent,
    });
    let mut body = vec![panel];
    if !is_known(name) {
        body.push(arguments_fold(arguments));
    }

    vec![Node::Collapse {
        header: header_runs(name, arguments, result, accent),
        body,
        default_open: false,
        show_background: false,
    }]
}

/// The status colour the row and the panel icon carry.
fn accent_role(result: Option<&ToolOutcome>) -> ColorRole {
    match result.map(|result| result.outcome.as_str()) {
        None => ColorRole::Muted,
        Some("executed") => ColorRole::Success,
        Some("denied") => ColorRole::Warning,
        Some(_) => ColorRole::Danger,
    }
}

/// The outcome glyph: a spinner while the call runs, else its result.
fn outcome_icon(result: Option<&ToolOutcome>) -> IconKind {
    match result.map(|result| result.outcome.as_str()) {
        None => IconKind::Spinner,
        Some("executed") => IconKind::CheckCircle,
        Some("denied") => IconKind::WarningCircle,
        Some(_) => IconKind::XCircle,
    }
}

/// The collapsed row: status glyph, verb, a dimmed summary, then the duration.
fn header_runs(
    name: &str,
    arguments: &Value,
    result: Option<&ToolOutcome>,
    accent: ColorRole,
) -> Vec<Run> {
    let mut runs = vec![
        Run::icon(outcome_icon(result), "  ", 13.0, accent),
        Run::text(tool_label(name), 13.0, ColorRole::Muted),
    ];
    if let Some(summary) = summarize(name, arguments) {
        runs.push(Run::text("  ", 13.0, ColorRole::Dim));
        runs.push(Run::text(shorten(&summary, 64), 13.0, ColorRole::Dim));
    }
    if let Some(result) = result {
        runs.push(Run::text(
            format!("  ·  {} ms", result.duration_ms),
            13.0,
            ColorRole::Dim,
        ));
    }
    runs
}

/// The pieces of a tool panel, gathered so the builder takes one argument.
struct Panel<'a> {
    title: &'a str,
    icon: IconKind,
    added: usize,
    removed: usize,
    copy: &'a str,
    lines: &'a [Line],
    running: bool,
    accent: ColorRole,
}

/// The framed panel: a title bar, a hairline, then the body.
fn panel_node(panel: &Panel<'_>) -> Node {
    Node::Frame {
        fill: Some(ColorRole::PanelBg),
        stroke: Some(ColorRole::Border),
        left_bar: None,
        radius: CornerSpec::same(PANEL_RADIUS),
        padding: EdgeInsets::zero(),
        stretch: true,
        child: Box::new(Node::column(vec![
            title_bar(
                panel.icon,
                panel.title,
                panel.added,
                panel.removed,
                panel.copy,
                panel.accent,
            ),
            Node::Rule,
            body_node(panel.lines, panel.running),
        ])),
    }
}

fn title_bar(
    icon: IconKind,
    title: &str,
    added: usize,
    removed: usize,
    copy: &str,
    accent: ColorRole,
) -> Node {
    let mut children = vec![
        Node::Icon {
            kind: icon,
            color: accent,
            size: TITLE_SIZE,
        },
        Node::text(vec![Run::text(title, TITLE_SIZE, ColorRole::Text)]),
    ];
    if added > 0 {
        children.push(Node::text(vec![Run::mono(
            format!("+{added}"),
            11.0,
            ColorRole::DiffAddFg,
        )]));
    }
    if removed > 0 {
        children.push(Node::text(vec![Run::mono(
            format!("-{removed}"),
            11.0,
            ColorRole::DiffDelFg,
        )]));
    }
    children.push(Node::Grow);
    children.push(Node::CopyButton {
        text: copy.to_string(),
        hover: "复制到剪贴板".to_string(),
    });

    Node::Frame {
        fill: Some(ColorRole::PanelHeaderBg),
        stroke: None,
        left_bar: None,
        radius: CornerSpec {
            nw: PANEL_RADIUS,
            ne: PANEL_RADIUS,
            sw: 0.0,
            se: 0.0,
        },
        padding: EdgeInsets::symmetric(10.0, 6.0),
        stretch: true,
        child: Box::new(Node::Row {
            children,
            gap: 8.0,
            align: VerticalAlign::Center,
        }),
    }
}

fn body_node(lines: &[Line], running: bool) -> Node {
    if running {
        return Node::Frame {
            fill: None,
            stroke: None,
            left_bar: None,
            radius: CornerSpec::same(0.0),
            padding: EdgeInsets::symmetric(10.0, 8.0),
            stretch: false,
            child: Box::new(Node::text(vec![Run::text(
                "运行中…",
                CODE_SIZE,
                ColorRole::Muted,
            )])),
        };
    }

    Node::Scroll {
        max_height: PANEL_MAX_HEIGHT,
        both_axes: true,
        child: Box::new(Node::Frame {
            fill: None,
            stroke: None,
            left_bar: None,
            radius: CornerSpec::same(0.0),
            padding: EdgeInsets::symmetric(10.0, 8.0),
            stretch: false,
            child: Box::new(Node::code(body_runs(lines))),
        }),
    }
}

/// The body as one run stream: a muted gutter number, a marker, the text, then
/// a newline that carries no tint.
fn body_runs(lines: &[Line]) -> Vec<Run> {
    let mut runs = Vec::new();
    for line in lines {
        let (marker, color, background) = match line.kind {
            LineKind::Add => ("+ ", ColorRole::DiffAddFg, Some(ColorRole::DiffAddBg)),
            LineKind::Del => ("- ", ColorRole::DiffDelFg, Some(ColorRole::DiffDelBg)),
            LineKind::Hunk | LineKind::Meta => (MARKER_WIDTH, ColorRole::Muted, None),
            LineKind::Plain => (MARKER_WIDTH, ColorRole::Text, None),
        };
        if let Some(num) = line.num {
            runs.push(Run::mono(format!("{num:>4}  "), CODE_SIZE, ColorRole::Muted));
        }
        let mut run = Run::mono(format!("{marker}{}", line.text), CODE_SIZE, color);
        run.background = background;
        runs.push(run);
        runs.push(Run::mono("\n", CODE_SIZE, ColorRole::Text));
    }
    runs
}

/// The raw arguments, behind a fold, for a tool this build does not shape.
fn arguments_fold(arguments: &Value) -> Node {
    Node::Collapse {
        header: vec![Run::text("参数", 11.0, ColorRole::Muted)],
        body: vec![Node::text(vec![Run::mono(
            pretty(arguments),
            CODE_SIZE,
            ColorRole::Text,
        )])],
        default_open: false,
        show_background: false,
    }
}
