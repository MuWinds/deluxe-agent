//! 后台任务和 subagent 列表。

use eframe::egui;
use egui::{Color32, RichText};

use crate::icons;
use crate::ipc::{JobState, JobView};
use crate::theme::{self, Palette};

use super::common;

/// 任务行的交互事件。
#[derive(Default)]
pub struct JobRowClick {
    /// 用户点击了打开按钮（仅对 subagent 有效）。
    pub open: bool,
    /// 用户点击了停止按钮。
    pub stop: bool,
}

/// 绘制单个任务行。
pub fn draw_job_row(ui: &mut egui::Ui, p: &Palette, job: &JobView, selected: bool) -> JobRowClick {
    let (icon, tag) = if job.is_subagent() {
        (icons::ROBOT, "子代理")
    } else {
        (icons::TERMINAL_WINDOW, "后台任务")
    };
    let colour = job_colour(job, p);
    let mut click = JobRowClick::default();

    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(theme::font(12.0)).color(colour));
        ui.label(
            RichText::new(tag)
                .size(theme::font(11.0))
                .color(p.text_muted)
                .strong(),
        );
        ui.label(
            RichText::new(common::shorten(&job.label, 60))
                .size(theme::font(11.0))
                .color(p.text),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !job.is_settled()
                && job_button(ui, icons::STOP_CIRCLE, false, "停止这个任务").clicked()
            {
                click.stop = true;
            }
            if job.is_subagent() && job_button(ui, icons::EYE, selected, "查看它的过程").clicked()
            {
                click.open = true;
            }
            ui.label(
                RichText::new(common::job_status_text(job))
                    .size(theme::font(11.0))
                    .color(colour),
            );
        });
    });

    let mut hover = format!("{} · {}", job.id, job.label);
    if let Some(detail) = &job.detail {
        hover.push('\n');
        hover.push_str(detail);
    }
    ui.response().on_hover_text(hover);

    click
}

/// 任务操作按钮。
pub fn job_button(ui: &mut egui::Ui, icon: &str, selected: bool, tooltip: &str) -> egui::Response {
    ui.add_sized(
        [20.0, 18.0],
        egui::Button::selectable(selected, RichText::new(icon).size(theme::font(11.0)))
            .frame(false),
    )
    .on_hover_text(tooltip)
}

/// 根据任务状态返回颜色。
pub fn job_colour(job: &JobView, p: &Palette) -> Color32 {
    match &job.state {
        JobState::Running => theme::OK_GREEN,
        JobState::Stopping => theme::WARN_AMBER,
        JobState::Completed => p.text_muted,
        JobState::Killed => theme::WARN_AMBER,
        JobState::Failed => theme::BAD_RED,
    }
}
