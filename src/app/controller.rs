//! Plugin UI intent handling and event folding without GUI resources.

use std::path::Path;

use crate::ipc::{Cmd, Event};
use crate::plugins::ui_protocol::{validate_action, validate_document, PluginUiAction};

use super::view_model::PluginSurfaceView;
use super::App;

impl App {
    pub(super) fn open_plugin_surface(&mut self, plugin_id: String, surface_id: String) {
        let project = self.displayed_project();
        let allowed = super::view_model::plugin_surfaces(&self.catalogue, Path::new(&project))
            .iter()
            .any(|entry| entry.plugin_id == plugin_id && entry.surface_id == surface_id);
        if !allowed {
            return;
        }
        self.close_plugin_surface();
        let request = crate::plugins::ui_protocol::SurfaceRequest {
            plugin_id,
            project: project.into(),
            surface_id,
            request_id: self.next_config_request_id,
        };
        self.next_config_request_id = self.next_config_request_id.wrapping_add(1);
        let failed = self
            .cmd_tx
            .send(Cmd::OpenPluginSurface(request.clone()))
            .is_err();
        self.plugin_surface = Some(PluginSurfaceView {
            request,
            document: None,
            error: failed.then(|| "Plugin worker is unavailable".into()),
            busy: !failed,
            closed: failed,
        });
    }

    pub(super) fn close_plugin_surface(&mut self) {
        if let Some(surface) = self.plugin_surface.take() {
            let _ = self.cmd_tx.send(Cmd::ClosePluginSurface(surface.request));
        }
    }

    pub(super) fn plugin_ui_action(&mut self, action: PluginUiAction) {
        let Some(surface) = self.plugin_surface.as_mut() else {
            return;
        };
        if surface.busy || surface.closed || surface.request != action.surface {
            return;
        }
        let Some(document) = &surface.document else {
            return;
        };
        if validate_action(document, &action).is_err() {
            return;
        }
        surface.busy = true;
        if self.cmd_tx.send(Cmd::PluginUiAction(action)).is_err() {
            surface.busy = false;
            surface.error = Some("Plugin worker is unavailable".into());
        }
    }

    pub(super) fn fold_plugin_ui(&mut self, event: &Event) -> bool {
        let request = match event {
            Event::PluginUiUpdated { request, .. }
            | Event::PluginUiClosed { request }
            | Event::PluginUiFailed { request, .. } => request,
            _ => return false,
        };
        let project = self.displayed_project();
        let enabled = self
            .catalogue
            .for_project(Path::new(&project))
            .into_iter()
            .find(|plugin| plugin.id == request.plugin_id)
            .and_then(|plugin| plugin.manifest.wasm_runtime());
        let Some(surface) = self.plugin_surface.as_mut().filter(|surface| {
            surface.request == *request
                && request.project == Path::new(&project)
                && enabled
                    .as_ref()
                    .is_some_and(|manifest| manifest.ui.surfaces.contains(&request.surface_id))
        }) else {
            return true;
        };
        match event {
            Event::PluginUiUpdated { document, .. } if !surface.closed => {
                let previous = surface.document.as_ref().map(|document| document.revision);
                let actions = enabled
                    .map(|manifest| manifest.ui.actions)
                    .unwrap_or_default();
                if validate_document(document, request, &actions, previous).is_ok() {
                    surface.document = Some(document.clone());
                    surface.busy = false;
                    surface.error = None;
                } else {
                    surface.busy = false;
                    surface.error = Some("Plugin returned an invalid UI snapshot".into());
                }
            }
            Event::PluginUiFailed { message, .. } => {
                surface.error = Some(message.clone());
                surface.busy = false;
            }
            Event::PluginUiClosed { .. } => {
                surface.closed = true;
                surface.busy = false;
            }
            _ => {}
        }
        true
    }

    pub(super) fn reconcile_plugin_surface(&mut self) {
        if self
            .plugin_surface
            .as_ref()
            .is_some_and(|surface| surface.request.project != Path::new(&self.displayed_project()))
        {
            self.close_plugin_surface();
        }
    }
}
