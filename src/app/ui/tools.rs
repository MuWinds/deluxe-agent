//! 工具卡片绘制

use eframe::egui;
use egui::text::LayoutJob;
use egui::{FontId, Frame, RichText};

use crate::icons;
use crate::renderer::present;
use crate::renderer::protocol::Node;
use crate::session::ToolResult;
use crate::theme::{self, Palette};

use super::common::{outcome_colour, outcome_icon};
use super::primitives::selectable_code;
use super::widgets::append_run;

/// One tool call as the card draws it.
pub(super) struct ToolCard<'a> {
    pub(super) call_id: &'a str,
    pub(super) name: &'a str,
    pub(super) result: Option<&'a ToolResult>,
    pub(super) nodes: Option<&'a [Node]>,
}

/// One tool call.
pub(super) fn draw_tool_card(ui: &mut egui::Ui, p: &Palette, card: ToolCard<'_>, max_width: f32) {
    let ToolCard {
        call_id,
        name,
        result,
        nodes,
    } = card;

    ui.vertical_centered(|ui| {
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            match nodes {
                Some(nodes) => present::draw(ui, p, nodes, call_id),
                None => draw_tool_fallback(ui, p, call_id, name, result),
            }
        });
    });
    ui.add_space(8.0);
}

/// The stand-in for a tool card the renderer could not shape: a fold named after
/// the tool, holding its raw output.
fn draw_tool_fallback(
    ui: &mut egui::Ui,
    p: &Palette,
    call_id: &str,
    name: &str,
    result: Option<&ToolResult>,
) {
    let accent = match result {
        None => p.text_muted,
        Some(result) => outcome_colour(result.outcome),
    };
    let glyph = match result {
        None => icons::SPINNER_GAP,
        Some(result) => outcome_icon(result.outcome),
    };
    let font = FontId::proportional(theme::font(13.0));
    let mut job = LayoutJob::default();
    append_run(&mut job, &format!("{glyph}  "), &font, accent);
    append_run(&mut job, name, &font, p.text_muted);

    egui::CollapsingHeader::new(job)
        .id_salt(call_id)
        .default_open(false)
        .show_background(false)
        .show(ui, |ui| match result {
            Some(result) if !result.output.is_empty() => selectable_code(ui, &result.output),
            Some(_) => {
                ui.label(
                    RichText::new("（无输出）")
                        .size(theme::font(12.0))
                        .color(p.text_muted),
                );
            }
            None => {
                ui.label(
                    RichText::new("运行中…")
                        .size(theme::font(12.0))
                        .color(p.text_muted),
                );
            }
        });
}
