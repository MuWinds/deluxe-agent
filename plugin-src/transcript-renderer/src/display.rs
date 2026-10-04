//! The display list the host draws.
//!
//! The renderer's output is a tree of generic presentation nodes — styled text
//! runs, stacks, indents, frames, rules, scroll areas, folds, icons, copy
//! buttons, alignment boxes and grids. Nothing here names Markdown or a tool
//! panel: the host walks this tree and never learns what a heading or a patch
//! is. This module mirrors the host's `renderer::protocol` and is `Serialize`
//! only.

use serde::Serialize;

/// A semantic colour the host resolves against its palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ColorRole {
    Text,
    Muted,
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

/// A font family the host resolves and scales.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FontRole {
    Proportional,
    Monospace,
}

/// Horizontal placement of a box or a grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HorizontalAlign {
    Left,
    Center,
    Right,
}

/// Vertical placement of a row's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VerticalAlign {
    Top,
    Center,
}

/// Per-corner radii of a frame, in design units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CornerSpec {
    pub nw: f32,
    pub ne: f32,
    pub sw: f32,
    pub se: f32,
}

impl CornerSpec {
    pub fn same(radius: f32) -> Self {
        Self {
            nw: radius,
            ne: radius,
            sw: radius,
            se: radius,
        }
    }
}

/// A frame's inset on each side, in design units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeInsets {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl EdgeInsets {
    pub fn symmetric(x: f32, y: f32) -> Self {
        Self {
            left: x,
            right: x,
            top: y,
            bottom: y,
        }
    }

    pub fn zero() -> Self {
        Self {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        }
    }
}

/// A one-side border, painted over a frame's edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Border {
    pub width: f32,
    pub color: ColorRole,
}

/// One styled run of text.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub text: String,
    pub font: FontRole,
    pub size: f32,
    pub color: ColorRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<ColorRole>,
    pub italic: bool,
    pub underline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconKind>,
}

impl Run {
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

    pub fn mono(text: impl Into<String>, size: f32, color: ColorRole) -> Self {
        Self {
            font: FontRole::Monospace,
            ..Self::text(text, size, color)
        }
    }

    pub fn icon(kind: IconKind, text: impl Into<String>, size: f32, color: ColorRole) -> Self {
        Self {
            icon: Some(kind),
            ..Self::text(text, size, color)
        }
    }

    pub fn tinted(mut self, role: ColorRole) -> Self {
        self.background = Some(role);
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn underlined(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn linked(mut self, url: impl Into<String>) -> Self {
        self.link = Some(url.into());
        self
    }
}

/// One column of a grid.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GridColumn {
    pub align: HorizontalAlign,
    pub width: f32,
}

/// The semantic glyph a node or run asks for; the host maps it to a glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// One node of the display list.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Node {
    Text {
        runs: Vec<Run>,
        wrap: bool,
        selectable: bool,
    },
    Column {
        children: Vec<Node>,
    },
    Row {
        children: Vec<Node>,
        gap: f32,
        align: VerticalAlign,
    },
    Indent {
        amount: f32,
        child: Box<Node>,
    },
    Frame {
        #[serde(skip_serializing_if = "Option::is_none")]
        fill: Option<ColorRole>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stroke: Option<ColorRole>,
        #[serde(skip_serializing_if = "Option::is_none")]
        left_bar: Option<Border>,
        radius: CornerSpec,
        padding: EdgeInsets,
        stretch: bool,
        child: Box<Node>,
    },
    Rule,
    Spacer {
        size: f32,
    },
    Grow,
    Scroll {
        max_height: f32,
        both_axes: bool,
        child: Box<Node>,
    },
    Collapse {
        header: Vec<Run>,
        body: Vec<Node>,
        default_open: bool,
        show_background: bool,
    },
    Icon {
        kind: IconKind,
        color: ColorRole,
        size: f32,
    },
    CopyButton {
        text: String,
        hover: String,
    },
    Align {
        align: HorizontalAlign,
        #[serde(skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
        child: Box<Node>,
    },
    Grid {
        columns: Vec<GridColumn>,
        gap: f32,
        header: Vec<Vec<Run>>,
        rows: Vec<Vec<Vec<Run>>>,
        rules: bool,
    },
}

impl Node {
    /// A wrapped, selectable text node.
    pub fn text(runs: Vec<Run>) -> Self {
        Node::Text {
            runs,
            wrap: true,
            selectable: true,
        }
    }

    /// A non-wrapping text node (code slabs and diff bodies).
    pub fn code(runs: Vec<Run>) -> Self {
        Node::Text {
            runs,
            wrap: false,
            selectable: true,
        }
    }

    pub fn column(children: Vec<Node>) -> Self {
        Node::Column { children }
    }

    pub fn row(children: Vec<Node>, gap: f32) -> Self {
        Node::Row {
            children,
            gap,
            align: VerticalAlign::Top,
        }
    }

    pub fn spacer(size: f32) -> Self {
        Node::Spacer { size }
    }
}
