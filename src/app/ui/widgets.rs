//! 可复用 UI 组件工具函数。

use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, TextureHandle};

use std::collections::HashMap;

use crate::attachments::ImageRef;

/// LayoutJob 追加样式化文本。
pub fn append_run(job: &mut LayoutJob, text: &str, font: &FontId, colour: Color32) {
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

/// 将图片加载为缩略图纹理。
pub fn transcript_thumb(
    thumbs: &mut HashMap<String, TextureHandle>,
    ctx: &egui::Context,
    image: &ImageRef,
) -> Option<TextureHandle> {
    use crate::attachments;
    use crate::image_ops;

    if let Some(cached) = thumbs.get(&image.id) {
        return Some(cached.clone());
    }

    let bytes = attachments::load_bytes(image).ok()?;
    let raster = if image.media_type == "image/jpeg" {
        image_ops::decode_jpeg(&bytes).ok()?
    } else {
        image_ops::decode_png(&bytes).ok()?
    };
    let texture = ctx.load_texture(
        format!("thumb://{}", image.id),
        egui::ColorImage::from_rgba_unmultiplied(
            [raster.width as usize, raster.height as usize],
            &raster.rgba,
        ),
        egui::TextureOptions::default(),
    );
    thumbs.insert(image.id.clone(), texture.clone());
    Some(texture)
}
