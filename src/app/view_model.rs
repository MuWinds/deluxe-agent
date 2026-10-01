//! Read-only project-scoped projections consumed by the plugin renderer.

use crate::plugins::ui_protocol::{PluginUiDocument, SurfaceRequest};
use crate::plugins::PluginCatalogue;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(super) struct PluginSurfaceEntry {
    pub(super) plugin_id: String,
    pub(super) display_name: String,
    pub(super) surface_id: String,
}

#[derive(Debug, Clone)]
pub(super) struct PluginSurfaceView {
    pub(super) request: SurfaceRequest,
    pub(super) document: Option<Arc<PluginUiDocument>>,
    pub(super) error: Option<String>,
    pub(super) busy: bool,
    pub(super) closed: bool,
}

/// Projects enabled surface entries without loading components or reading files.
pub(super) fn plugin_surfaces(
    catalogue: &PluginCatalogue,
    project: &Path,
) -> Vec<PluginSurfaceEntry> {
    catalogue
        .for_project(project)
        .into_iter()
        .flat_map(|plugin| {
            plugin
                .manifest
                .wasm_runtime()
                .into_iter()
                .flat_map(|manifest| manifest.ui.surfaces)
                .map(|surface_id| PluginSurfaceEntry {
                    plugin_id: plugin.id.clone(),
                    display_name: plugin.display_name().into(),
                    surface_id,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
