//! 会话列表侧边栏。

use eframe::egui;
use egui::{Atom, RichText};
use uuid::Uuid;

use crate::ipc::RunState;
use crate::session::{self, Session};
use crate::theme::{self, Palette};

use super::common;

/// 导航栏圆形图标按钮。
pub fn rail_button(ui: &mut egui::Ui, icon: &str, selected: bool, tooltip: &str) -> egui::Response {
    ui.add_sized(
        [36.0, 36.0],
        egui::Button::selectable(selected, RichText::new(icon).size(theme::font(17.0))),
    )
    .on_hover_text(tooltip)
}

/// 侧边栏通用行容器。
pub fn sidebar_row(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: &str,
    label: &str,
    selected: bool,
    muted: bool,
) -> egui::Response {
    let text = RichText::new(format!("{icon}  {label}")).size(theme::font(13.0));
    let text = if muted && !selected {
        text.color(p.text_muted)
    } else {
        text
    };

    ui.add_sized(
        [ui.available_width(), 28.0],
        egui::Button::selectable(selected, (text, Atom::grow())).truncate(),
    )
}

/// 单个会话行。
pub fn session_row(
    ui: &mut egui::Ui,
    p: &Palette,
    session: &Session,
    selected: Option<Uuid>,
    now: u64,
    nested: bool,
) -> egui::Response {
    let indent = if nested { "    " } else { "" };
    let label = format!(
        "{indent}{}",
        common::shorten(&session.title(), if nested { 26 } else { 32 })
    );

    let text = RichText::new(label).size(theme::font(13.0));
    let text = if selected == Some(session.id) {
        text
    } else {
        text.color(p.text_muted)
    };

    let marker = match session.state {
        RunState::Running => "●",
        RunState::Finished | RunState::Failed => "",
    };

    let response = ui.add_sized(
        [ui.available_width(), 26.0],
        egui::Button::selectable(selected == Some(session.id), (text, Atom::grow()))
            .truncate(),
    );

    if !marker.is_empty() {
        let rect = response.rect;
        ui.painter().circle_filled(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            3.5,
            if session.state == RunState::Running {
                p.accent
            } else {
                crate::theme::BAD_RED
            },
        );
    }

    response.on_hover_text(format!(
        "{}\n{}",
        session.title(),
        session::age_label(session.created_at, now)
    ))
}
