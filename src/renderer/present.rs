//! The generic display-list renderer.
//!
//! Walks the renderer's nodes and emits egui widgets. It knows nothing about
//! Markdown or tool panels: every node is a generic presentation primitive, and
//! every colour and font is a role resolved against the palette. Scroll areas
//! and folds take their egui id from their path in the tree, so the renderer
//! never has to invent a salt.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use eframe::egui;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::text_selection::LabelSelectionState;
use egui::{
    Align, Button, Color32, CornerRadius, FontId, Frame, Galley, Layout, Margin, RichText,
    ScrollArea, Sense, Stroke, Vec2,
};

use crate::icons;
use crate::renderer::protocol::{
    Border, ColorRole, CornerSpec, EdgeInsets, FontRole, HorizontalAlign, IconKind, Node, Run,
    VerticalAlign,
};
use crate::theme::{self, Palette};

/// The space above and below a grid rule.
const GRID_V_PAD: f32 = 4.0;

/// Draws a display list down the current `Ui`.
///
/// `salt` names every scroll area and fold, folded with each node's path in the
/// tree, so they stay stable across frames and unique per message.
pub fn draw(ui: &mut egui::Ui, p: &Palette, nodes: &[Node], salt: impl Hash) {
    let base = salt_of(salt);
    let mut path = Vec::new();
    // The list carries its own gaps, so the stack must not add any of its own.
    ui.spacing_mut().item_spacing.y = 0.0;
    draw_nodes(ui, p, nodes, base, &mut path);
}

fn salt_of(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn draw_nodes(ui: &mut egui::Ui, p: &Palette, nodes: &[Node], base: u64, path: &mut Vec<usize>) {
    for (index, node) in nodes.iter().enumerate() {
        path.push(index);
        draw_node(ui, p, node, base, path);
        path.pop();
    }
}

fn draw_node(ui: &mut egui::Ui, p: &Palette, node: &Node, base: u64, path: &mut Vec<usize>) {
    match node {
        Node::Text {
            runs,
            wrap,
            selectable,
        } => draw_text(ui, p, runs, *wrap, *selectable),
        Node::Column { children } => {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                draw_nodes(ui, p, children, base, path);
            });
        }
        Node::Row {
            children,
            gap,
            align,
        } => draw_row(ui, p, children, *gap, *align, base, path),
        Node::Indent { amount, child } => {
            ui.add_space(*amount);
            draw_node(ui, p, child, base, path);
        }
        Node::Frame {
            fill,
            stroke,
            left_bar,
            radius,
            padding,
            stretch,
            child,
        } => draw_frame(
            ui, p, *fill, *stroke, *left_bar, *radius, *padding, *stretch, child, base, path,
        ),
        Node::Rule => {
            ui.separator();
        }
        Node::Spacer { size } => {
            ui.add_space(*size);
        }
        Node::Grow => {
            // Reached only when a row did not special-case it; consume the room.
            let remaining = ui.available_width();
            ui.add_space(remaining);
        }
        Node::Scroll {
            max_height,
            both_axes,
            child,
        } => {
            let area = if *both_axes {
                ScrollArea::both()
            } else {
                ScrollArea::vertical()
            };
            // Lay the scroll out against a definite region of its own. The
            // parent is often the transcript's own scroll area, which hands its
            // content a `max_rect` only one viewport tall — so a node below the
            // first viewport sees a zero or negative available height, and egui
            // would collapse this nested scroll area to its 64 px minimum
            // instead of sizing to the content. `max_height` still caps it.
            let region = egui::Rect::from_min_size(
                ui.cursor().min,
                Vec2::new(ui.available_width(), *max_height),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(region), |ui| {
                area.id_salt((base, path.clone()))
                    .max_height(*max_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| draw_node(ui, p, child, base, path));
            });
        }
        Node::Collapse {
            header,
            body,
            default_open,
            show_background,
        } => {
            let job = header_job(p, header);
            egui::CollapsingHeader::new(job)
                .id_salt((base, path.clone()))
                .default_open(*default_open)
                .show_background(*show_background)
                .show(ui, |ui| draw_nodes(ui, p, body, base, path));
        }
        Node::Icon { kind, color, size } => {
            ui.label(
                RichText::new(icon_glyph(*kind))
                    .size(theme::font(*size))
                    .color(resolve_color(p, *color)),
            );
        }
        Node::CopyButton { text, hover } => draw_copy_button(ui, p, text, hover),
        Node::Align {
            align,
            width,
            child,
        } => {
            let width = width.unwrap_or_else(|| ui.available_width());
            let cross = match align {
                HorizontalAlign::Left => Align::Min,
                HorizontalAlign::Center => Align::Center,
                HorizontalAlign::Right => Align::Max,
            };
            ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(cross), |ui| {
                draw_node(ui, p, child, base, path);
            });
        }
        Node::Grid {
            columns,
            gap,
            header,
            rows,
            rules,
        } => draw_grid(ui, p, columns, *gap, header, rows, *rules),
    }
}

/// A row, with any tail after a `Grow` pushed to the far edge.
fn draw_row(
    ui: &mut egui::Ui,
    p: &Palette,
    children: &[Node],
    gap: f32,
    align: VerticalAlign,
    base: u64,
    path: &mut Vec<usize>,
) {
    let cross = match align {
        VerticalAlign::Top => Align::Min,
        VerticalAlign::Center => Align::Center,
    };
    // Bound the row to one line's height and let it grow with its content.
    // `with_layout` alone hands the row the whole available height, and a
    // centred cross axis then makes it fill that height — a card's title bar
    // became a screen-tall band. `ui.horizontal` bounds itself the same way.
    let size = Vec2::new(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(size, Layout::left_to_right(cross), |ui| {
        ui.spacing_mut().item_spacing.x = gap;
        let grow = children.iter().position(|node| matches!(node, Node::Grow));
        let Some(grow) = grow else {
            draw_nodes(ui, p, children, base, path);
            return;
        };
        for (index, node) in children[..grow].iter().enumerate() {
            path.push(index);
            draw_node(ui, p, node, base, path);
            path.pop();
        }
        // The tail is laid out from the right, so its first node sits at the
        // far edge — which is what a trailing button wants.
        ui.with_layout(Layout::right_to_left(cross), |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for index in (grow + 1..children.len()).rev() {
                path.push(index);
                draw_node(ui, p, &children[index], base, path);
                path.pop();
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_frame(
    ui: &mut egui::Ui,
    p: &Palette,
    fill: Option<ColorRole>,
    stroke: Option<ColorRole>,
    left_bar: Option<Border>,
    radius: CornerSpec,
    padding: EdgeInsets,
    stretch: bool,
    child: &Node,
    base: u64,
    path: &mut Vec<usize>,
) {
    let mut frame = Frame::NONE
        .corner_radius(corner(radius))
        .inner_margin(margin(padding));
    if let Some(role) = fill {
        frame = frame.fill(resolve_color(p, role));
    }
    if let Some(role) = stroke {
        frame = frame.stroke(Stroke::new(1.0, resolve_color(p, role)));
    }
    let inner = frame.show(ui, |ui| {
        if stretch {
            ui.set_min_width(ui.available_width());
        }
        draw_node(ui, p, child, base, path);
    });
    if let Some(bar) = left_bar {
        let rect = inner.response.rect;
        ui.painter().vline(
            rect.left(),
            rect.y_range(),
            Stroke::new(bar.width, resolve_color(p, bar.color)),
        );
    }
}

fn draw_text(ui: &mut egui::Ui, p: &Palette, runs: &[Run], wrap: bool, selectable: bool) {
    if runs.iter().any(|run| run.link.is_some()) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for run in runs {
                let mut job = LayoutJob::default();
                job.append(&run_text(run), 0.0, run_format(p, run));
                job.wrap.break_anywhere = true;
                match &run.link {
                    Some(url) => {
                        ui.hyperlink_to(job, url);
                    }
                    None => {
                        ui.add(egui::Label::new(job).wrap().selectable(selectable));
                    }
                }
            }
        });
        return;
    }

    let mut job = LayoutJob::default();
    if !wrap {
        job.wrap = TextWrapping::no_max_width();
    }
    for run in runs {
        job.append(&run_text(run), 0.0, run_format(p, run));
    }
    let label = egui::Label::new(job).selectable(selectable);
    if wrap {
        ui.add(label.wrap());
    } else {
        ui.add(label);
    }
}

fn draw_copy_button(ui: &mut egui::Ui, p: &Palette, text: &str, hover: &str) {
    let response = ui
        .add(
            Button::new(
                RichText::new(icons::COPY)
                    .size(theme::font(12.0))
                    .color(p.text_muted),
            )
            .frame(false),
        )
        .on_hover_text(hover);
    if response.clicked() {
        ui.ctx().copy_text(text.to_string());
    }
}

fn draw_grid(
    ui: &mut egui::Ui,
    p: &Palette,
    columns: &[crate::renderer::protocol::GridColumn],
    gap: f32,
    header: &[Vec<Run>],
    rows: &[Vec<Vec<Run>>],
    rules: bool,
) {
    if columns.is_empty() {
        return;
    }
    let widths: Vec<f32> = columns.iter().map(|column| column.width).collect();
    let total: f32 = widths.iter().sum::<f32>() + gap * (columns.len() - 1) as f32;
    ui.allocate_ui_with_layout(Vec2::new(total, 0.0), Layout::top_down(Align::Min), |ui| {
        grid_row(ui, p, columns, &widths, gap, header);
        ui.add_space(GRID_V_PAD);
        grid_rule(ui, p, total);
        ui.add_space(GRID_V_PAD);
        for (index, row) in rows.iter().enumerate() {
            grid_row(ui, p, columns, &widths, gap, row);
            if rules && index < rows.len() - 1 {
                ui.add_space(GRID_V_PAD);
                grid_rule(ui, p, total);
                ui.add_space(GRID_V_PAD);
            }
        }
    });
}

fn grid_row(
    ui: &mut egui::Ui,
    p: &Palette,
    columns: &[crate::renderer::protocol::GridColumn],
    widths: &[f32],
    gap: f32,
    cells: &[Vec<Run>],
) {
    let laid: Vec<Option<CellText>> = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| layout_cell(ui, p, cell, *width))
        .collect();
    let height = laid
        .iter()
        .flatten()
        .fold(0.0f32, |tallest, cell| tallest.max(cell.height));

    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (index, cell) in laid.iter().enumerate() {
            let width = widths[index];
            // Cells are positioned by hand so the columns line up, but the text
            // still has to be registered with egui's label selection: a directly
            // painted galley is invisible to drag-select and copy.
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(width, height.max(1.0)), Sense::click_and_drag());
            let Some(cell) = cell else { continue };
            let left = match columns[index].align {
                HorizontalAlign::Left => 0.0,
                HorizontalAlign::Center => (width - cell.width) * 0.5,
                HorizontalAlign::Right => width - cell.width,
            }
            .max(0.0);
            let top = ((height - cell.height) * 0.5).max(0.0);
            if ui.is_rect_visible(response.rect) {
                LabelSelectionState::label_text_selection(
                    ui,
                    &response,
                    rect.left_top() + Vec2::new(left, top),
                    cell.galley.clone(),
                    p.text,
                    Stroke::NONE,
                );
            }
        }
    });
}

fn grid_rule(ui: &mut egui::Ui, p: &Palette, width: f32) {
    let y = ui.cursor().top() + 0.5;
    let x = ui.cursor().left();
    ui.painter()
        .hline(x..=x + width, y, Stroke::new(1.0, p.border));
    ui.add_space(1.0);
}

struct CellText {
    galley: Arc<Galley>,
    width: f32,
    height: f32,
}

fn layout_cell(ui: &egui::Ui, p: &Palette, runs: &[Run], width: f32) -> Option<CellText> {
    if runs
        .iter()
        .all(|run| run.text.is_empty() && run.icon.is_none())
    {
        return None;
    }
    let mut job = LayoutJob::default();
    job.wrap.break_anywhere = true;
    job.wrap.max_width = width;
    for run in runs {
        job.append(&run_text(run), 0.0, run_format(p, run));
    }
    let galley = ui.painter().layout_job(job);
    Some(CellText {
        width: galley.size().x,
        height: galley.size().y,
        galley,
    })
}

/// The header of a fold, as one multi-coloured galley.
fn header_job(p: &Palette, runs: &[Run]) -> LayoutJob {
    let mut job = LayoutJob::default();
    for run in runs {
        job.append(&run_text(run), 0.0, run_format(p, run));
    }
    job
}

/// The text a run draws: an icon run is prefixed with its glyph.
fn run_text(run: &Run) -> String {
    match run.icon {
        Some(kind) => format!("{}{}", icon_glyph(kind), run.text),
        None => run.text.clone(),
    }
}

fn run_format(p: &Palette, run: &Run) -> TextFormat {
    let color = resolve_color(p, run.color);
    TextFormat {
        font_id: font_id(run.font, run.size),
        color,
        italics: run.italic,
        underline: if run.underline {
            Stroke::new(1.0, color)
        } else {
            Stroke::NONE
        },
        background: run
            .background
            .map(|role| resolve_color(p, role))
            .unwrap_or(Color32::TRANSPARENT),
        expand_bg: if run.background == Some(ColorRole::CodeBg) {
            2.0
        } else {
            1.0
        },
        ..Default::default()
    }
}

fn font_id(font: FontRole, size: f32) -> FontId {
    let size = theme::font(size);
    match font {
        FontRole::Proportional => FontId::proportional(size),
        FontRole::Monospace => FontId::monospace(size),
    }
}

/// The one place a role becomes a colour.
fn resolve_color(p: &Palette, role: ColorRole) -> Color32 {
    match role {
        ColorRole::Text => p.text,
        ColorRole::Muted => p.text_muted,
        ColorRole::Dim => p.text_muted.gamma_multiply(0.72),
        ColorRole::Accent => p.accent,
        ColorRole::Success => theme::OK_GREEN,
        ColorRole::Warning => theme::WARN_AMBER,
        ColorRole::Danger => theme::BAD_RED,
        ColorRole::DiffAddFg => p.diff_add_fg,
        ColorRole::DiffDelFg => p.diff_del_fg,
        ColorRole::DiffAddBg => p.diff_add_bg,
        ColorRole::DiffDelBg => p.diff_del_bg,
        ColorRole::CodeBg => p.code_bg,
        ColorRole::PanelBg => p.code_bg,
        ColorRole::PanelHeaderBg => p.code_header_bg,
        ColorRole::Border => p.border,
    }
}

/// The one place a semantic icon becomes a glyph.
pub fn icon_glyph(kind: IconKind) -> &'static str {
    match kind {
        IconKind::NotePencil => icons::NOTE_PENCIL,
        IconKind::Terminal => icons::TERMINAL_WINDOW,
        IconKind::File => icons::MAGNIFYING_GLASS,
        IconKind::Folder => icons::FOLDER_SIMPLE,
        IconKind::Image => icons::IMAGE,
        IconKind::Dots => icons::DOTS_THREE,
        IconKind::StopCircle => icons::STOP_CIRCLE,
        IconKind::Gear => icons::GEAR,
        IconKind::CheckCircle => icons::CHECK_CIRCLE,
        IconKind::WarningCircle => icons::WARNING_CIRCLE,
        IconKind::XCircle => icons::X_CIRCLE,
        IconKind::Spinner => icons::SPINNER_GAP,
    }
}

fn corner(radius: CornerSpec) -> CornerRadius {
    CornerRadius {
        nw: radius.nw.max(0.0) as u8,
        ne: radius.ne.max(0.0) as u8,
        sw: radius.sw.max(0.0) as u8,
        se: radius.se.max(0.0) as u8,
    }
}

fn margin(padding: EdgeInsets) -> Margin {
    Margin {
        left: padding.left.max(0.0) as i8,
        right: padding.right.max(0.0) as i8,
        top: padding.top.max(0.0) as i8,
        bottom: padding.bottom.max(0.0) as i8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row must take only its content height, not fill the space it is given.
    #[test]
    fn a_centered_row_does_not_fill_the_height() {
        let ctx = egui::Context::default();
        let p = theme::palette(crate::theme::ThemeChoice::Dark);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let nodes = vec![Node::Row {
            children: vec![
                Node::Text {
                    runs: vec![Run::text("root", 12.0, ColorRole::Text)],
                    wrap: false,
                    selectable: true,
                },
                Node::Grow,
                Node::CopyButton {
                    text: "x".into(),
                    hover: "copy".into(),
                },
            ],
            gap: 8.0,
            align: VerticalAlign::Center,
        }];
        let mut height = 0.0f32;
        let mut output = ctx.run_ui(input, |ui| {
            ui.set_width(800.0);
            let top = ui.cursor().top();
            draw(ui, &p, &nodes, "row");
            height = ui.min_rect().bottom() - top;
        });
        output.textures_delta.clear();
        assert!(height < 40.0, "the row filled {height} px");
    }

    /// A scroll node must size to its content even when the surrounding layout
    /// has no height left. That is the case for a tool-card body below the
    /// transcript's first viewport: the parent scroll area hands its content a
    /// `max_rect` only one viewport tall, so every later row sees a
    /// non-positive available height. egui then collapses a nested `ScrollArea`
    /// to its 64 px minimum, which is why the card read as too short.
    #[test]
    fn a_scroll_sizes_to_its_content_with_no_room_left() {
        let ctx = egui::Context::default();
        let p = theme::palette(crate::theme::ThemeChoice::Dark);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(600.0, 400.0),
            )),
            ..Default::default()
        };
        let body: String = (0..40).map(|index| format!("line {index}\n")).collect();
        let nodes = vec![
            // Burn the whole viewport so the scroll sees no room at all.
            Node::Spacer { size: 2000.0 },
            Node::Scroll {
                max_height: 320.0,
                both_axes: true,
                child: Box::new(Node::Text {
                    runs: vec![Run::text(body, 12.0, ColorRole::Text)],
                    wrap: false,
                    selectable: true,
                }),
            },
        ];
        let mut height = 0.0f32;
        let mut output = ctx.run_ui(input, |ui| {
            ui.set_width(600.0);
            let top = ui.cursor().top();
            draw(ui, &p, &nodes, "test");
            height = ui.min_rect().bottom() - top - 2000.0;
        });
        output.textures_delta.clear();
        assert!(
            height > 200.0,
            "the scroll should size to its content, drew {height} px"
        );
    }

    #[test]
    fn a_short_scroll_shrinks_to_its_content() {
        let ctx = egui::Context::default();
        let p = theme::palette(crate::theme::ThemeChoice::Dark);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(600.0, 400.0),
            )),
            ..Default::default()
        };
        let nodes = vec![Node::Scroll {
            max_height: 320.0,
            both_axes: true,
            child: Box::new(Node::Text {
                runs: vec![Run::text("one line\n", 12.0, ColorRole::Text)],
                wrap: false,
                selectable: true,
            }),
        }];
        let mut height = 0.0f32;
        let mut output = ctx.run_ui(input, |ui| {
            ui.set_width(600.0);
            let top = ui.cursor().top();
            draw(ui, &p, &nodes, "test");
            height = ui.min_rect().bottom() - top;
        });
        output.textures_delta.clear();
        assert!(
            height < 64.0,
            "a short scroll should shrink to its content, drew {height} px"
        );
    }

    /// A whole card — panel frame, title bar, and a scroll body — must keep its
    /// listing inside the panel and stay about title + `max_height` tall.
    #[test]
    fn a_card_body_keeps_its_listing_inside_the_panel() {
        let ctx = egui::Context::default();
        let p = theme::palette(crate::theme::ThemeChoice::Dark);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let listing: String = std::iter::once("root — 33 entries\n".to_string())
            .chain((0..33).map(|index| format!("file{index}.txt\t{} bytes\n", index * 97)))
            .collect();
        let title_bar = Node::Frame {
            fill: Some(ColorRole::PanelHeaderBg),
            stroke: None,
            left_bar: None,
            radius: CornerSpec::default(),
            padding: EdgeInsets {
                left: 10.0,
                right: 10.0,
                top: 6.0,
                bottom: 6.0,
            },
            stretch: true,
            child: Box::new(Node::Row {
                children: vec![
                    Node::Text {
                        runs: vec![Run::text("root", 12.0, ColorRole::Text)],
                        wrap: false,
                        selectable: true,
                    },
                    Node::Grow,
                    Node::CopyButton {
                        text: "x".into(),
                        hover: "copy".into(),
                    },
                ],
                gap: 8.0,
                align: VerticalAlign::Center,
            }),
        };
        let body = Node::Scroll {
            max_height: 320.0,
            both_axes: true,
            child: Box::new(Node::Frame {
                fill: None,
                stroke: None,
                left_bar: None,
                radius: CornerSpec::default(),
                padding: EdgeInsets {
                    left: 10.0,
                    right: 10.0,
                    top: 8.0,
                    bottom: 8.0,
                },
                stretch: false,
                child: Box::new(Node::Text {
                    runs: vec![Run::text(listing, 12.0, ColorRole::Text)],
                    wrap: false,
                    selectable: true,
                }),
            }),
        };
        let panel = Node::Frame {
            fill: Some(ColorRole::PanelBg),
            stroke: Some(ColorRole::Border),
            left_bar: None,
            radius: CornerSpec::default(),
            padding: EdgeInsets::default(),
            stretch: true,
            child: Box::new(Node::Column {
                children: vec![title_bar, Node::Rule, body],
            }),
        };
        let nodes = vec![Node::Collapse {
            header: vec![Run::text("列出了目录", 13.0, ColorRole::Muted)],
            body: vec![panel],
            default_open: true,
            show_background: false,
        }];

        let mut output = ctx.run_ui(input, |ui| {
            ui.set_width(800.0);
            draw(ui, &p, &nodes, "card");
        });
        let mut panel_rect = None;
        let mut title_rect = None;
        for clipped in &output.shapes {
            if let egui::Shape::Rect(rect) = &clipped.shape {
                if rect.fill == p.code_bg {
                    panel_rect = Some(rect.rect);
                } else if rect.fill == p.code_header_bg {
                    title_rect = Some(rect.rect);
                }
            }
        }
        output.textures_delta.clear();
        let panel = panel_rect.expect("the panel frame should be painted");
        let title = title_rect.expect("the title bar should be painted");
        assert!(
            title.height() < 40.0,
            "the title bar filled the card: {} px",
            title.height()
        );
        assert!(
            panel.height() < 400.0,
            "the card is taller than title + body cap: {} px",
            panel.height()
        );
    }
}
