//! 可复用 UI 组件。

use eframe::egui;
use egui::{Align, Color32, CornerRadius, FontId, Frame, Margin, RichText, TextureHandle, Vec2};
use egui::text::LayoutJob;

use std::collections::HashMap;

use crate::attachments::ImageRef;
use crate::icons;
use crate::theme::{self, Palette};

// COMPOSER_BUTTON 在 impl.rs 中定义
const COMPOSER_BUTTON: f32 = 30.0;

/// 圆形图标按钮。
pub fn circle_button<'a>(icon: &'a str, p: &Palette) -> egui::Button<'a> {
    egui::Button::new(RichText::new(icon).size(theme::font(15.0)).color(p.main_bg))
        .fill(p.text)
        .corner_radius(CornerRadius::same(COMPOSER_BUTTON as u8 / 2))
        .min_size(Vec2::splat(COMPOSER_BUTTON))
}

/// 分组标签。
pub fn section_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .size(theme::font(11.0))
            .color(p.text_muted),
    );
    ui.add_space(2.0);
}

/// Logo 绘制。
pub fn draw_logo(ui: &mut egui::Ui, p: &Palette) {
    let size = 56.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let radius = CornerRadius::same(14);
    ui.painter().rect_filled(rect, radius, p.hover_bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        icons::TERMINAL_WINDOW,
        egui::FontId::proportional(theme::font(26.0)),
        p.text_muted,
    );
}

/// 空状态占位。
pub fn draw_empty_state(ui: &mut egui::Ui, p: &Palette, project: Option<&str>) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.20).max(20.0));
        draw_logo(ui, p);
        ui.add_space(20.0);
        ui.set_max_width(620.0);
        let text = match project {
            Some(name) => format!("你想让我们在 {name} 中构建什么?"),
            None => "你想让我们构建什么?".to_string(),
        };
        ui.add(
            egui::Label::new(RichText::new(text).size(theme::font(24.0)).color(p.text))
                .wrap()
                .halign(Align::Center),
        );
    });
}

/// 可选择的等宽文本。
pub fn selectable_code(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).monospace().size(theme::font(12.0)))
            .wrap()
            .selectable(true),
    );
}

/// 附件 chip 移除按钮。
pub fn remove_chip(ui: &mut egui::Ui, p: &Palette, id: &str, image: &ImageRef) -> egui::Response {
    ui.push_id(id, |ui| {
        Frame::NONE
            .fill(p.hover_bg)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(8, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(icons::IMAGE)
                            .size(theme::font(12.0))
                            .color(p.text_muted),
                    );
                    ui.label(
                        RichText::new(chip_label(image))
                            .size(theme::font(12.0))
                            .color(p.text),
                    );
                    ui.add(
                        egui::Button::new(RichText::new(icons::X).size(theme::font(11.0)))
                            .frame(false),
                    )
                    .on_hover_text("移除")
                })
                .inner
            })
            .inner
    })
    .inner
}

/// 附件 chip 标签文本。
fn chip_label(image: &ImageRef) -> String {
    image.name.as_deref().unwrap_or("已上传").to_string()
}

/// LayoutJob 追加样式化文本。
pub fn append_run(job: &mut LayoutJob, text: &str, font: &FontId, colour: Color32) {
    use egui::TextFormat;
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font.clone(),
            color: colour,
            ..Default::default()
        },
    );
}

/// 对话记录缩略图尺寸。
pub const TRANSCRIPT_THUMB: (f32, f32) = (180.0, 130.0);

/// 将图片加载为缩略图纹理。
pub fn transcript_thumb(
    ctx: &egui::Context,
    salt: (uuid::Uuid, usize),
    image: &ImageRef,
    thumbs: &mut HashMap<String, TextureHandle>,
) -> Option<TextureHandle> {
    use crate::attachments;
    use crate::image_ops;

    let key = format!("{}:{}", salt.0, image.id);

    if let Some(handle) = thumbs.get(&key) {
        return Some(handle.clone());
    }

    let bytes = attachments::load_bytes(image).ok()?;
    let raster = if image.media_type == "image/jpeg" {
        image_ops::decode_jpeg(&bytes).ok()?
    } else {
        image_ops::decode_png(&bytes).ok()?
    };

    let handle = ctx.load_texture(
        &key,
        egui::ColorImage::from_rgba_unmultiplied(
            [raster.width as usize, raster.height as usize],
            &raster.rgba,
        ),
        egui::TextureOptions::LINEAR,
    );
    thumbs.insert(key, handle.clone());
    Some(handle)
}
