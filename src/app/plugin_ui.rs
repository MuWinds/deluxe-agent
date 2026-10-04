//! Snapshot rendering knows egui, never the component runtime.

use eframe::egui;

use crate::app::parse_tokens;
use crate::plugins::ui_protocol::{
    PluginUiAction, PluginUiDocument, SurfaceRequest, TextEmphasis, TextReader, UiNode, UiValue,
};

use super::intents::UiIntent;
use super::App;

/// Where a text input holds the text the user is typing.
///
/// A pure renderer cannot own edit state, and rebuilding the field from the
/// snapshot on every commit is what made it impossible to finish a value: a
/// snapshot answering an earlier keystroke arrives mid-word and overwrites it.
/// The draft is the field's own memory between keystrokes.
fn token_draft_key(control_id: &str) -> egui::Id {
    egui::Id::new(("plugin-ui-token-draft", control_id))
}

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
            reader,
            action,
        } => {
            text_input(
                ui,
                request,
                revision,
                id,
                value,
                placeholder.as_deref(),
                *reader,
                action,
                intents,
            );
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

/// Draws one text input and, on an edit worth committing, queues its action.
///
/// A plain input sends its text on every edit. A token input waits for the
/// field to lose focus, because a half-typed count is not a value and because
/// a snapshot answering an earlier keystroke would otherwise rewrite the field
/// under the cursor; its draft also outlives that round trip, so typing `1M`
/// can actually be finished.
#[allow(clippy::too_many_arguments)]
fn text_input(
    ui: &mut egui::Ui,
    request: &SurfaceRequest,
    revision: u64,
    control_id: &str,
    value: &str,
    placeholder: Option<&str>,
    reader: TextReader,
    action: &str,
    intents: &mut Vec<UiIntent>,
) {
    let placeholder = placeholder.unwrap_or_default();
    match reader {
        TextReader::Text => {
            let mut text = value.to_owned();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .id_salt(control_id)
                    .desired_width(ui.available_width().min(320.0))
                    .hint_text(placeholder),
            );
            if response.changed() {
                push_action(
                    intents,
                    request,
                    revision,
                    control_id,
                    action,
                    Some(UiValue::String(text)),
                );
            }
        }
        TextReader::Tokens => {
            // A committed change is sent on blur, not on each keystroke: a
            // half-typed count is not a value, and a snapshot answering an
            // earlier keystroke would otherwise rewrite the field under the
            // cursor. While the field has focus the draft is authoritative, so
            // it is kept the moment it gains focus and every frame thereafter;
            // once it loses focus the committed value takes over again.
            let draft_key = token_draft_key(control_id);
            let field_id = ui.make_persistent_id(control_id);
            let focused = ui.memory(|memory| memory.has_focus(field_id));
            let stored = ui.ctx().data(|data| data.get_temp::<String>(draft_key));
            let mut text = if focused {
                stored.unwrap_or_else(|| format_tokens(value))
            } else {
                format_tokens(value)
            };
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .id_salt(control_id)
                    .desired_width(ui.available_width().min(320.0))
                    .hint_text(placeholder),
            );
            if response.changed() {
                ui.ctx()
                    .data_mut(|data| data.insert_temp(draft_key, text.clone()));
            }
            if response.lost_focus() {
                // An empty field commits an empty value, which is how an
                // optional limit is cleared; a partial number keeps its draft
                // and is not committed, so the user can come back and fix it.
                let committed = if text.trim().is_empty() {
                    Some(String::new())
                } else {
                    parse_tokens(&text).map(|tokens| tokens.to_string())
                };
                if let Some(value) = committed {
                    ui.ctx().data_mut(|data| data.remove::<String>(draft_key));
                    push_action(
                        intents,
                        request,
                        revision,
                        control_id,
                        action,
                        Some(UiValue::String(value)),
                    );
                }
            } else if ui.memory(|memory| memory.has_focus(field_id)) {
                // Keep the draft alive across idle frames, so the field never
                // flashes the committed value under the cursor.
                ui.ctx().data_mut(|data| data.insert_temp(draft_key, text));
            }
        }
    }
}

/// Renders a token count the way the composer's gauge does.
fn format_tokens(value: &str) -> String {
    // An empty field is not a zero: it is an unset optional limit, and it has
    // to stay visibly empty so it can be cleared.
    if value.trim().is_empty() {
        String::new()
    } else {
        crate::app::format_tokens(parse_tokens(value).unwrap_or(0))
    }
}
