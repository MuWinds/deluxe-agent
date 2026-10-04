//! 基础 UI 组件：按钮、行、标签等可复用的小部件

use eframe::egui;
use egui::{Align, CornerRadius, RichText, Stroke, Vec2};

use crate::icons;
use crate::theme::{self, Palette};

/// Width of the far-left icon rail.
pub(super) const RAIL_WIDTH: f32 = 52.0;

/// Default width of the session sidebar.
pub(super) const SIDEBAR_WIDTH: f32 = 300.0;

/// Height of the round buttons in the composer.
pub(super) const COMPOSER_BUTTON: f32 = 30.0;

pub(super) fn rail_button(
    ui: &mut egui::Ui,
    icon: &str,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    ui.add_sized(
        [36.0, 36.0],
        egui::Button::selectable(selected, RichText::new(icon).size(theme::font(17.0))),
    )
    .on_hover_text(tooltip)
}

pub(super) fn sidebar_row(
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
        egui::Button::selectable(selected, (text, egui::Atom::grow())).truncate(),
    )
}

pub(super) fn section_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .size(theme::font(11.0))
            .color(p.text_muted),
    );
    ui.add_space(2.0);
}

pub(super) fn circle_button<'a>(icon: &'a str, p: &Palette) -> egui::Button<'a> {
    egui::Button::new(RichText::new(icon).size(theme::font(15.0)).color(p.main_bg))
        .fill(p.text)
        .corner_radius(CornerRadius::same(COMPOSER_BUTTON as u8 / 2))
        .min_size(Vec2::splat(COMPOSER_BUTTON))
}

pub(super) fn draw_logo(ui: &mut egui::Ui, p: &Palette) {
    let size = 56.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let radius = CornerRadius::same(14);
    ui.painter().rect_filled(rect, radius, p.hover_bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, p.border),
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

pub(super) fn draw_empty_state(ui: &mut egui::Ui, p: &Palette, project: Option<&str>) {
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

pub(super) fn selectable_code(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).monospace().size(theme::font(12.0)))
            .wrap()
            .selectable(true),
    );
}
