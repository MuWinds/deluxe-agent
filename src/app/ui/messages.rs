//! 消息绘制：气泡、代理消息、推理块

use std::collections::HashMap;
use std::hash::Hash;

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, TextureHandle, Vec2};
use uuid::Uuid;

use crate::attachments::ImageRef;
use crate::icons;
use crate::renderer::present;
use crate::renderer::protocol::Node;
use crate::theme::{self, Palette};

use super::super::attachment_caption;
use super::primitives::selectable_code;
use super::widgets::transcript_thumb;

/// The margin the chat column keeps from the panel's edges, shared by the
/// transcript and the composer so their columns start and end on the same x.
pub const CHAT_MARGIN_X: f32 = 16.0;

/// Gap between a right-hugging bubble and the column's right edge, so the
/// bubble does not sit flush against the edge the replies stop at.
pub const BUBBLE_EDGE_GAP: f32 = 14.0;

/// Horizontal padding inside a bubble, between its edge and the text.
const BUBBLE_PADDING_X: f32 = 12.0;

/// What a user-message attachment renders as in the transcript.
const TRANSCRIPT_THUMB: (f32, f32) = (180.0, 130.0);

/// One transcript message as the drawing code needs it.
pub struct Message<'a> {
    pub text: &'a str,
    pub nodes: Option<&'a [Node]>,
    pub salt: (Uuid, usize),
}

/// The assistant's reply, in a centred column.
pub(super) fn draw_agent_message(
    ui: &mut egui::Ui,
    p: &Palette,
    message: Message<'_>,
    max_width: f32,
) {
    let Message { text, nodes, salt } = message;
    ui.vertical_centered(|ui| {
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            ui.vertical(|ui| {
                draw_message_body(ui, p, text, nodes, salt);
            });
        });
    });
    ui.add_space(8.0);
}

/// Draws a message body from the renderer's display list when it is ready, else
/// as plain text.
fn draw_message_body<S: Hash>(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    nodes: Option<&[Node]>,
    salt: S,
) {
    match nodes {
        Some(nodes) => present::draw(ui, p, nodes, salt),
        None => draw_plain_text(ui, p, text),
    }
}

/// The fallback for a message the renderer could not shape: the raw body,
/// selectable and wrapped, with no Markdown applied.
fn draw_plain_text(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).size(theme::font(14.0)).color(p.text))
            .wrap()
            .selectable(true),
    );
}

/// One message, as a bubble of rendered content.
pub fn draw_bubble(
    ui: &mut egui::Ui,
    p: &Palette,
    message: Message<'_>,
    fill: Color32,
    side: Align,
    max_width: f32,
) -> egui::Rect {
    let Message { text, nodes, salt } = message;
    let band = match side {
        Align::Max => max_width - BUBBLE_EDGE_GAP,
        _ => max_width,
    };
    let band_rect = egui::Rect::from_min_size(
        egui::pos2(ui.cursor().left(), ui.cursor().top()),
        egui::vec2(band, f32::INFINITY),
    );

    // Pass one, invisible and measure-only
    let measured = {
        let mut probe = ui.new_child(
            egui::UiBuilder::new()
                .invisible()
                .sizing_pass()
                .max_rect(egui::Rect::from_min_size(
                    band_rect.min,
                    egui::vec2(band * 2.0, f32::INFINITY),
                ))
                .layout(Layout::left_to_right(Align::Min)),
        );
        Frame::NONE
            .fill(fill)
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::symmetric(BUBBLE_PADDING_X as i8, 9))
            .show(&mut probe, |frame| {
                frame.vertical(|ui| {
                    draw_message_body(ui, p, text, nodes, (salt, "measure"));
                });
            });
        probe.min_rect().width().min(band)
    };

    // Pass two: the visible bubble
    let x = match side {
        Align::Max => band_rect.left() + band - measured,
        Align::Center => band_rect.left() + (band - measured) / 2.0,
        _ => band_rect.left(),
    };
    let rect = egui::Rect::from_min_max(
        egui::pos2(x, band_rect.top()),
        egui::pos2(x + measured, band_rect.bottom()),
    );
    let mut drawn = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Min)),
    );
    Frame::NONE
        .fill(fill)
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(BUBBLE_PADDING_X as i8, 9))
        .show(&mut drawn, |frame| {
            frame.set_min_width(measured - 2.0 * BUBBLE_PADDING_X);
            frame.vertical(|ui| {
                draw_message_body(ui, p, text, nodes, salt);
            });
        });
    ui.advance_cursor_after_rect(egui::Rect::from_min_size(
        egui::pos2(band_rect.left(), band_rect.top()),
        egui::vec2(band, drawn.min_rect().height()),
    ));
    ui.add_space(8.0);
    drawn.min_rect()
}

/// The model's chain of thought, collapsed until clicked.
pub(super) fn draw_reasoning_block(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    expanded: bool,
    max_width: f32,
) -> bool {
    let marker = if expanded {
        icons::CARET_DOWN
    } else {
        icons::CARET_RIGHT
    };
    let header = format!("{marker}  {} 思维链", icons::BRAIN);

    let mut clicked = false;
    ui.vertical_centered(|ui| {
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            Frame::NONE
                .fill(p.bubble_assistant)
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        let response = ui
                            .add(
                                egui::Button::new(
                                    RichText::new(header)
                                        .size(theme::font(13.0))
                                        .color(p.text_muted),
                                )
                                .frame(false),
                            )
                            .on_hover_text(if expanded {
                                "点击折叠"
                            } else {
                                "点击展开模型的思考过程"
                            });
                        clicked = response.clicked();
                        if expanded {
                            selectable_code(ui, text);
                        }
                    });
                });
        });
    });
    ui.add_space(8.0);

    clicked
}

/// The images one user step was sent with, as thumbnails under its bubble.
pub(super) fn draw_user_images(
    ui: &mut egui::Ui,
    salt: (Uuid, usize),
    images: &[ImageRef],
    thumbs: &mut HashMap<String, TextureHandle>,
) {
    if images.is_empty() {
        return;
    }
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        ui.add_space(10.0);
        for (image_index, image) in images.iter().enumerate() {
            let Some(texture) = transcript_thumb(thumbs, ui.ctx(), image) else {
                tracing::warn!(id = %image.id, "an attached image could no longer be rendered");
                continue;
            };
            ui.push_id((salt, image_index), |ui| {
                ui.add(
                    egui::Image::from_texture(&texture)
                        .max_size(Vec2::new(TRANSCRIPT_THUMB.0, TRANSCRIPT_THUMB.1))
                        .corner_radius(CornerRadius::same(8)),
                )
                .on_hover_text(attachment_caption(image));
            });
        }
    });
}
