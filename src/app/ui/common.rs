//! 共享工具函数。

use std::hash::{Hash, Hasher};

use eframe::egui;
use egui::{Color32, FontId};
use serde_json::Value;

use crate::ipc::{AuditOutcome, JobView};
use crate::renderer::protocol::{HunkLines, RenderMetrics, ToolRenderResult};
use crate::session::ToolResult;
use crate::theme;

// COMPOSER_MAX_WIDTH 在 impl.rs 中定义
const COMPOSER_MAX_WIDTH: f32 = 820.0;

/// 计算渲染度量。
pub fn render_metrics(ui: &egui::Ui, width: f32) -> RenderMetrics {
    let font = FontId::proportional(theme::font(14.0));
    let char_width = ui
        .painter()
        .layout_no_wrap("0".to_string(), font, Color32::PLACEHOLDER)
        .size()
        .x;
    let width = if width.is_finite() {
        width
    } else {
        COMPOSER_MAX_WIDTH
    };
    RenderMetrics {
        available_width: width.clamp(0.0, COMPOSER_MAX_WIDTH),
        char_width: char_width.max(1.0),
        column_gap: 12.0,
    }
}

/// 计算工具调用指纹用于去重。
pub fn tool_fingerprint(name: &str, arguments: &Value, result: Option<&ToolResult>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    arguments.to_string().hash(&mut hasher);
    if let Some(result) = result {
        serde_json::to_string(result)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// 将 ToolResult 转换为渲染协议格式。
pub fn tool_result(result: Option<ToolResult>) -> Option<ToolRenderResult> {
    result.map(|result| ToolRenderResult {
        outcome: outcome_code(result.outcome).to_string(),
        output: result.output,
        hunks: result
            .hunks
            .into_iter()
            .map(|section| HunkLines {
                path: section.path,
                lines: section.lines,
            })
            .collect(),
        duration_ms: result.duration_ms,
    })
}

/// AuditOutcome 转字符串代码。
pub fn outcome_code(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Executed => "executed",
        AuditOutcome::Denied => "denied",
        AuditOutcome::Failed => "failed",
    }
}

/// AuditOutcome 对应的颜色。
pub fn outcome_colour(outcome: AuditOutcome) -> Color32 {
    match outcome {
        AuditOutcome::Executed => theme::OK_GREEN,
        AuditOutcome::Denied => theme::WARN_AMBER,
        AuditOutcome::Failed => theme::BAD_RED,
    }
}

/// AuditOutcome 对应的图标。
pub fn outcome_icon(outcome: AuditOutcome) -> &'static str {
    use crate::icons;
    match outcome {
        AuditOutcome::Executed => icons::CHECK_CIRCLE,
        AuditOutcome::Denied => icons::X_CIRCLE,
        AuditOutcome::Failed => icons::X_CIRCLE,
    }
}

/// 任务状态文本。
pub fn job_status_text(job: &JobView) -> String {
    job.state.label().to_string()
}

/// 截断长文本并扁平化换行。
pub fn shorten(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max).collect::<String>() + "…"
    }
}
