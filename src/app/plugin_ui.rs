//! Snapshot rendering knows egui, never the component runtime.

use eframe::egui;

use crate::plugins::ui_protocol::{
    PluginUiAction, PluginUiDocument, SurfaceRequest, TextEmphasis, UiNode, UiValue,
};

use super::intents::UiIntent;
use super::App;

pub(super) fn draw(ctx: &egui::Context, app: &App, intents: &mut Vec<UiIntent>) {
    let Some(surface) = &app.plugin_surface else {
        return;
    };
    let title = surface
        .document
        .as_ref()
        .map(|doc| doc.title.as_str())
        .unwrap_or(&surface.request.surface_id);
    let mut open = true;
    egui::Window::new(title)
        .id(egui::Id::new(("plugin-surface", &surface.request)))
        .open(&mut open)
        .collapsible(false)
        .default_width(420.0)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .max_height(480.0)
                .show(ui, |ui| {
                    if let Some(error) = &surface.error {
                        ui.colored_label(egui::Color32::from_rgb(192, 57, 43), error);
                    }
                    if surface.busy {
                        ui.spinner();
                    }
                    if surface.closed && ui.button("重新打开").clicked() {
                        intents.push(UiIntent::OpenPluginSurface {
                            plugin_id: surface.request.plugin_id.clone(),
                            surface_id: surface.request.surface_id.clone(),
                        });
                    }
                    if let Some(document) = &surface.document {
                        ui.add_enabled_ui(!surface.busy && !surface.closed, |ui| {
                            render(ui, document, &surface.request, intents);
                        });
                    }
                });
        });
    if !open {
        intents.push(UiIntent::ClosePluginSurface);
    }
}

/// Renders an immutable, host-validated tree and collects pure actions.
pub(super) fn render(
    ui: &mut egui::Ui,
    document: &PluginUiDocument,
    request: &SurfaceRequest,
    intents: &mut Vec<UiIntent>,
) {
    ui.push_id(request, |ui| {
        render_node(ui, &document.root, request, document.revision, intents);
    });
}

/// Renders one validated node, collecting pure actions. Shared with the
/// composer, which draws a plugin's control inline instead of in a window.
pub(super) fn render_node(
    ui: &mut egui::Ui,
    node: &UiNode,
    request: &SurfaceRequest,
    revision: u64,
    intents: &mut Vec<UiIntent>,
) {
    match node {
        UiNode::Empty => {}
        UiNode::Column { children } => {
            ui.vertical(|ui| {
                for child in children {
                    render_node(ui, child, request, revision, intents);
                }
            });
        }
        UiNode::Row { children } => {
            ui.horizontal_wrapped(|ui| {
                for child in children {
                    render_node(ui, child, request, revision, intents);
                }
            });
        }
        UiNode::Section {
            id,
            title,
            children,
        } => {
            egui::CollapsingHeader::new(title)
                .id_salt(id)
                .default_open(true)
                .show(ui, |ui| {
                    for child in children {
                        render_node(ui, child, request, revision, intents);
                    }
                });
        }
        UiNode::Text { text, emphasis } => {
            let text = match emphasis {
                TextEmphasis::Normal => egui::RichText::new(text),
                TextEmphasis::Strong => egui::RichText::new(text).strong(),
                TextEmphasis::Muted => egui::RichText::new(text).weak(),
            };
            ui.add(egui::Label::new(text).wrap());
        }
        UiNode::Button {
            id,
            label,
            action,
            enabled,
        } => {
            ui.push_id(id, |ui| {
                if ui
                    .add_enabled(*enabled, egui::Button::new(label).wrap())
                    .clicked()
                {
                    push_action(intents, request, revision, id, action, None);
                }
            });
        }
        UiNode::TextInput {
            id,
            value,
            placeholder,
            action,
        } => {
            let mut value = value.clone();
            if ui
                .add(
                    egui::TextEdit::singleline(&mut value)
                        .id_salt(id)
                        .desired_width(ui.available_width().min(320.0))
                        .hint_text(placeholder.as_deref().unwrap_or_default()),
                )
                .changed()
            {
                push_action(
                    intents,
                    request,
                    revision,
                    id,
                    action,
                    Some(UiValue::String(value)),
                );
            }
        }
        UiNode::Checkbox {
            id,
            label,
            value,
            action,
        } => {
            ui.push_id(id, |ui| {
                let mut value = *value;
                if ui.checkbox(&mut value, label).changed() {
                    push_action(
                        intents,
                        request,
                        revision,
                        id,
                        action,
                        Some(UiValue::Bool(value)),
                    );
                }
            });
        }
        UiNode::Select {
            id,
            value,
            options,
            action,
        } => {
            let mut selected = value.clone();
            let label = options
                .iter()
                .find(|option| &option.value == value)
                .map(|option| option.label.as_str())
                .unwrap_or(value);
            egui::ComboBox::from_id_salt(id)
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for option in options {
                        ui.selectable_value(&mut selected, option.value.clone(), &option.label);
                    }
                });
            if selected != *value {
                push_action(
                    intents,
                    request,
                    revision,
                    id,
                    action,
                    Some(UiValue::String(selected)),
                );
            }
        }
        UiNode::Progress { value, label } => {
            ui.add(egui::ProgressBar::new(*value).text(label.as_deref().unwrap_or_default()));
        }
        UiNode::Divider => {
            ui.separator();
        }
    }
}

fn push_action(
    intents: &mut Vec<UiIntent>,
    request: &SurfaceRequest,
    revision: u64,
    control_id: &str,
    action: &str,
    value: Option<UiValue>,
) {
    intents.push(UiIntent::PluginUiAction(PluginUiAction {
        surface: request.clone(),
        revision,
        control_id: control_id.into(),
        action: action.into(),
        value,
    }));
}
