//! 可复用 UI 组件工具函数。

use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

use crate::attachments::{self, Attachment};
use crate::icons;

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

/// 附件 chip 的图标：受支持的图片用图片图标，其余用通用文件图标。
pub fn attachment_icon(attachment: &Attachment) -> &'static str {
    let extension = std::path::Path::new(&attachment.path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if attachments::media_type_for_extension(extension).is_some() {
        icons::IMAGE
    } else {
        icons::FILE
    }
}
