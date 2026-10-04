//! Plugin UI intent handling and event folding without GUI resources.

use std::path::Path;

use crate::ipc::{Cmd, Event};
use crate::plugins::ui_protocol::{
    validate_action, validate_document, PluginUiAction, SurfaceRequest, COMPOSER_SURFACE_ID,
};
use crate::plugins::wasm_manifest::WasmManifest;

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
        let request = SurfaceRequest {
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
            // The composer's options live inside the same plugin, so a surface
            // that edited them may have changed what the composer should show.
            let _ = self.cmd_tx.send(Cmd::RefreshComposer);
        }
    }

    pub(super) fn plugin_ui_action(&mut self, action: PluginUiAction) {
        let composer = action.surface.surface_id == COMPOSER_SURFACE_ID;
        // Borrow the addressed field directly so `cmd_tx` stays independently
        // borrowable below.
        let slot = if composer {
            self.composer.as_mut()
        } else {
            self.plugin_surface.as_mut()
        };
        let Some(surface) = slot.filter(|surface| surface.request == action.surface) else {
            return;
        };
        if surface.busy || surface.closed {
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

        if request.surface_id == COMPOSER_SURFACE_ID {
            // The composer is global-scope, bound to the plugin's own
            // configuration root rather than the displayed project.
            if enabled
                .as_ref()
                .is_some_and(|manifest| manifest.ui.composer)
            {
                self.fold_composer(event, request, enabled.as_ref());
            }
            return true;
        }

        let actions = enabled
            .as_ref()
            .map(|manifest| manifest.ui.actions.clone())
            .unwrap_or_default();
        let allowed = enabled
            .as_ref()
            .is_some_and(|manifest| manifest.ui.surfaces.contains(&request.surface_id))
            && request.project == Path::new(&project);
        let Some(surface) = self.plugin_surface.as_mut().filter(|_| allowed) else {
            return true;
        };
        apply_surface_event(surface, event, request, &actions);
        true
    }

    /// Adopts and updates the composer from the worker's snapshots.
    ///
    /// A re-spawned composer arrives under a fresh request id; `Updated` adopts
    /// it, while a late `Closed`/`Failed` from the surface it replaced is
    /// dropped so the live composer is never marked closed.
    fn fold_composer(
        &mut self,
        event: &Event,
        request: &SurfaceRequest,
        enabled: Option<&WasmManifest>,
    ) {
        let current = self
            .composer
            .as_ref()
            .is_some_and(|surface| surface.request == *request);
        if !current {
            if !matches!(event, Event::PluginUiUpdated { .. }) {
                return;
            }
            self.composer = Some(PluginSurfaceView {
                request: request.clone(),
                document: None,
                error: None,
                busy: false,
                closed: false,
            });
        }
        // A disabled or uninstalled plugin drops its composer; the input row
        // then has no inline control until a later `Updated` re-adopts one.
        if matches!(event, Event::PluginUiClosed { .. }) {
            self.composer = None;
            return;
        }
        let actions = enabled
            .map(|manifest| manifest.ui.actions.clone())
            .unwrap_or_default();
        if let Some(surface) = self.composer.as_mut() {
            apply_surface_event(surface, event, request, &actions);
        }
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

/// Applies one worker event to a surface slot, re-validating the snapshot.
fn apply_surface_event(
    surface: &mut PluginSurfaceView,
    event: &Event,
    request: &SurfaceRequest,
    actions: &[String],
) {
    match event {
        Event::PluginUiUpdated { document, .. } if !surface.closed => {
            let previous = surface.document.as_ref().map(|document| document.revision);
            if validate_document(document, request, actions, previous).is_ok() {
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
}
